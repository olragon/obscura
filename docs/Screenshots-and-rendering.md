# Screenshots and rendering

Obscura can rasterize a page. `Page.captureScreenshot` returns real pixels, so
`page.screenshot()` works from Puppeteer and other CDP clients.

This is new: Obscura historically had no layout or paint engine and answered
that method with a descriptive "not supported" error.

## How it works

```
Page (obscura-dom, post-JavaScript)
        │  DomTree::outer_html(document)
        ▼
obscura-render
        ├──► blitz-html / blitz-dom     parse + style + layout (Stylo + Taffy)
        ├──► blitz-net                  fetch stylesheets, images, fonts
        ├──► blitz-paint + Parley       paint the scene, shape text
        └──► anyrender_vello_cpu        rasterize to an RGBA buffer (software)
        │
        ▼
   PNG or JPEG  ──► base64 ──► CDP `data`
```

The renderer is a separate crate (`obscura-render`) with no dependency on the
CDP or browser crates, so it is usable directly as a library and testable
without a server.

### Why the DOM is serialized rather than shared

`obscura-dom` and `blitz-dom` are independent tree implementations. Bridging
them node-for-node would be faster, but it would couple Obscura to Blitz's
internal mutation API, which is pre-1.0. Serializing to HTML and re-parsing
costs one O(document) pass per screenshot and keeps the seam at a single
documented boundary.

**What this means in practice:** the image reflects the **DOM at capture time**,
re-laid-out from scratch. Scroll position, focus, canvas contents, and state
that lives only in JS objects rather than in the DOM do not survive the
round-trip. Post-JavaScript DOM mutations *do* — that is the point.

### Software rasterization

The CPU backend (`anyrender_vello_cpu`) is deliberate. A headless server usually
has no GPU, and a software rasterizer produces identical bytes on every machine,
which is what makes screenshot output comparable across runs.

## Supported parameters

| Parameter | Support |
|---|---|
| `format` | `png` (default), `jpeg`. **`webp` is rejected** rather than silently answered with PNG. |
| `quality` | 1–100, JPEG only |
| `clip` | `{x, y, width, height}` in CSS px, applied after layout |
| `captureBeyondViewport` | full-page capture, bounded by `MAX_RENDER_HEIGHT` (16384 CSS px) |

Viewport and device pixel ratio come from `Emulation.setDeviceMetricsOverride`,
i.e. Puppeteer's `page.setViewport()`. `Page.getLayoutMetrics` reports the same
viewport, which matters because Playwright sizes its capture from it.

```js
await page.setViewport({ width: 1024, height: 768, deviceScaleFactor: 2 });
await page.screenshot({ path: 'out.png' });          // 2048x1536
await page.screenshot({ path: 'full.png', captureBeyondViewport: true });
```

## Limits and honest caveats

- **`Page.printToPDF` is still unsupported.** Layout works, but there is no
  vector-PDF paint backend. Wrapping a PNG in a PDF would defeat the reason
  clients ask for PDF (selectable text), so it errors instead.
- **`Page.captureSnapshot` (MHTML) is still unsupported** — it is a
  serialization format, not pixels.
- **Fidelity notes are logged, not returned.** `Page.captureScreenshot` has a
  fixed result shape, so caveats — a clamped full-page height, subresources that
  never arrived — go to the `obscura::render` tracing target at `warn`. Run with
  `RUST_LOG=obscura::render=debug` when a screenshot looks wrong; a truncated or
  asset-less image otherwise looks perfectly valid.
- **Blitz is a young engine.** It renders Hacker News, Wikipedia-class documents,
  and ordinary CSS layouts well, but it is not Blink. Expect gaps on heavy
  modern CSS.
- **Resource waiting is bounded** at 3s wall-clock. Assets slower than that are
  missing from the image, with a logged note.

## Safety bounds

Rendering allocates `width * height * 4` bytes from numbers an untrusted page
and an arbitrary client both influence, so the renderer bounds itself:

- `MAX_RENDER_WIDTH` 8192, `MAX_RENDER_HEIGHT` 16384 CSS px
- `MAX_RENDER_PIXELS` 64 MP total (≈256 MiB RGBA) — width and height can each be
  legal while their product is not
- The paint pass is wrapped in `catch_unwind`, matching Obscura's rule that a bad
  page degrades to an error instead of aborting the process
- Layout and raster run on the blocking pool, so a slow page does not stall the
  CDP dispatcher and starve other sessions

## Using the renderer directly

```rust
use obscura_render::{render_html, RenderOptions};
use std::time::Duration;

// NOTE: must run inside a Tokio runtime — blitz-net spawns fetch tasks.
let opts = RenderOptions { width: 1024, height: 768, ..Default::default() };
let out = render_html(&html, Some("https://example.com/"), &opts, Duration::from_secs(3))?;
std::fs::write("out.png", &out.bytes)?;
for note in &out.fidelity_notes {
    eprintln!("caveat: {note}");
}
```

There is also a diagnostic example for isolating whether a bad screenshot comes
from DOM serialization or from layout/paint:

```bash
cargo run --release -p obscura-render --example shot_file -- page.html https://example.com/ out.png
```
