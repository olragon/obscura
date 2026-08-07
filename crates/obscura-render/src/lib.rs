//! Layout + paint for Obscura.
//!
//! Obscura runs real JavaScript against its own DOM but has never had a layout
//! or paint engine, so `Page.captureScreenshot` and `Page.printToPDF` returned
//! a descriptive "not supported" error. This crate supplies the missing leg:
//! it takes the **post-JavaScript serialized DOM** and produces pixels via
//! [Blitz] (HTML/CSS layout, Stylo + Taffy underneath) rasterized by
//! `anyrender_vello_cpu`.
//!
//! # Why serialize the DOM instead of sharing it
//!
//! Obscura's DOM (`obscura-dom`) and Blitz's DOM (`blitz-dom`) are two
//! independent trees with different node representations. Bridging them
//! node-for-node would be the faster design, but it couples this crate to
//! Blitz's internal mutation API — which is pre-1.0 and still moving.
//! Serializing to HTML and re-parsing is O(document) once per screenshot and
//! keeps the seam to a single stable, documented boundary
//! (`DomTree::outer_html` on one side, `HtmlDocument::from_html` on the other).
//!
//! The honest consequence, which callers must know: what is rendered is the
//! **DOM state at capture time**, re-laid-out from scratch. Scroll position,
//! focus, canvas contents, and any state living only in JS objects rather than
//! in the DOM do not survive the round-trip. This is a real-fidelity limit, not
//! a bug to be fixed later by tuning — see [`RenderOutcome::fidelity_notes`].
//!
//! # Determinism
//!
//! The CPU backend is chosen deliberately over the GPU one. A headless server
//! usually has no GPU, and more importantly a software rasterizer produces the
//! same bytes on every machine, which is what makes screenshot output
//! comparable in a test suite at all.
//!
//! [Blitz]: https://github.com/DioxusLabs/blitz

use std::sync::Arc;

use anyrender::{render_to_buffer, PaintScene as _};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{util::Color, DocumentConfig};
use blitz_html::HtmlDocument;
use blitz_net::Provider;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::kurbo::Rect;
use peniko::Fill;

/// Hard ceiling on rendered height, in CSS pixels, before scaling.
///
/// A full-page capture of a hostile or infinite-scroll page can otherwise ask
/// for an allocation large enough to OOM the process. Obscura's whole
/// robustness posture is "one page cannot take down the process", so the
/// renderer bounds its own allocation rather than trusting the document.
pub const MAX_RENDER_HEIGHT: f64 = 16_384.0;

/// Hard ceiling on rendered width, in CSS pixels, before scaling.
pub const MAX_RENDER_WIDTH: u32 = 8_192;

/// Ceiling on total pixels (width * height, after scale) — ~64 MP, i.e. 256 MiB
/// of RGBA. Width and height can each be individually legal while their product
/// is not.
pub const MAX_RENDER_PIXELS: u64 = 64_000_000;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("viewport is degenerate: {0}")]
    BadViewport(String),
    #[error(
        "requested render area {width}x{height} exceeds the {max} pixel cap; \
         reduce the viewport or disable full-page capture"
    )]
    TooLarge { width: u32, height: u32, max: u64 },
    #[error("image encoding failed: {0}")]
    Encode(String),
    #[error("the paint pass panicked: {0}")]
    PaintPanic(String),
}

/// Output image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    /// Quality is carried on [`RenderOptions::jpeg_quality`].
    Jpeg,
}

impl ImageFormat {
    /// Parse a CDP `Page.captureScreenshot` `format` parameter.
    ///
    /// CDP also defines `webp`; we do not encode it, and returning PNG under a
    /// `webp` request would be a silent lie, so the caller is expected to reject
    /// it before reaching here. `None` maps to CDP's documented default (PNG).
    pub fn from_cdp(s: Option<&str>) -> Option<Self> {
        match s {
            None | Some("png") => Some(Self::Png),
            Some("jpeg") | Some("jpg") => Some(Self::Jpeg),
            _ => None,
        }
    }
}

/// A rectangular region to crop to, in CSS pixels, pre-scale.
#[derive(Debug, Clone, Copy)]
pub struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Viewport width in CSS pixels.
    pub width: u32,
    /// Viewport height in CSS pixels. Also the minimum output height.
    pub height: u32,
    /// Device pixel ratio. Output pixels = CSS pixels * scale.
    pub scale: f32,
    /// Grow the output to the document's full computed height, bounded by
    /// [`MAX_RENDER_HEIGHT`]. This is CDP's `captureBeyondViewport`.
    pub full_page: bool,
    pub format: ImageFormat,
    /// 1..=100. Ignored for PNG.
    pub jpeg_quality: u8,
    /// Painted before the document. Pages that set no background of their own
    /// would otherwise composite onto uninitialized-looking transparency.
    pub background: Color,
    pub color_scheme: ColorScheme,
    /// Optional crop, applied after layout.
    pub clip: Option<Clip>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            scale: 1.0,
            full_page: false,
            format: ImageFormat::Png,
            jpeg_quality: 80,
            background: Color::WHITE,
            color_scheme: ColorScheme::Light,
            clip: None,
        }
    }
}

/// A rendered image plus the things the caller should not have to guess about.
#[derive(Debug)]
pub struct RenderOutcome {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    /// Non-fatal fidelity caveats that applied to *this* render — e.g. the
    /// height was clamped, or subresources were still in flight when the
    /// fetch budget ran out.
    ///
    /// These are surfaced rather than logged-and-forgotten on purpose: a
    /// screenshot that is silently truncated or silently missing its images
    /// looks correct and is not, which is the single most expensive failure
    /// mode a rendering pipeline has.
    pub fidelity_notes: Vec<String>,
}

/// Render serialized HTML to an image.
///
/// `base_url` resolves relative subresources; without it, relative `<img>` and
/// stylesheet URLs cannot be fetched and the page will render unstyled.
///
/// `resource_timeout` bounds how long we wait for subresources (stylesheets,
/// images, fonts) to arrive. Blitz fetches them lazily during layout, so this
/// is the knob that trades screenshot fidelity against latency — and it must be
/// bounded, or a page with a hanging asset renders forever.
///
/// # Panics / requirements
///
/// Must be called **inside a Tokio runtime context** — `blitz-net`'s fetch
/// provider spawns tasks and panics with "there is no reactor running"
/// otherwise. From the CDP handler this holds because `spawn_blocking` threads
/// inherit the runtime context; from a plain `fn main` it does not.
///
/// It is a **wall-clock** budget on purpose. An earlier version counted
/// `document.resolve()` passes instead, which is the wrong unit: 32 passes over
/// a small document complete in ~150 ms — less than a single network
/// round-trip — so every external stylesheet lost the race and pages rendered
/// completely unstyled while the pass counter looked generous. Waiting on the
/// network is a time problem, so the budget is time.
pub fn render_html(
    html: &str,
    base_url: Option<&str>,
    opts: &RenderOptions,
    resource_timeout: std::time::Duration,
) -> Result<RenderOutcome, RenderError> {
    let mut notes = Vec::new();

    if opts.width == 0 || opts.height == 0 {
        return Err(RenderError::BadViewport(format!(
            "{}x{}",
            opts.width, opts.height
        )));
    }
    if !opts.scale.is_finite() || opts.scale <= 0.0 {
        return Err(RenderError::BadViewport(format!("scale={}", opts.scale)));
    }

    let width = opts.width.min(MAX_RENDER_WIDTH);
    if width != opts.width {
        notes.push(format!(
            "viewport width clamped from {} to {} px",
            opts.width, width
        ));
    }
    let scale = opts.scale;

    let net = Arc::new(Provider::new(None));
    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: base_url.map(str::to_owned),
            net_provider: Some(Arc::clone(&net) as _),
            viewport: Some(Viewport::new(
                (width as f32 * scale) as u32,
                (opts.height as f32 * scale) as u32,
                scale,
                opts.color_scheme,
            )),
            ..Default::default()
        },
    );

    // Resolve styles/layout, pumping until subresources settle or the budget is
    // spent. `Provider::is_empty()` means nothing is in flight.
    //
    // The sleep is load-bearing, not politeness: the fetches are driven by the
    // Tokio runtime's worker threads, and this function runs synchronously (on
    // a blocking thread when called from the CDP handler). A tight spin here
    // burns the budget in milliseconds without ever giving the network time to
    // deliver, which is precisely how external stylesheets got dropped.
    let deadline = std::time::Instant::now() + resource_timeout;
    let mut waited_for_resources = false;
    loop {
        document.resolve(0.0);
        if net.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            notes.push(format!(
                "subresource fetches still in flight after {:?}; image may be missing \
                 late-loading assets (stylesheets, images, fonts)",
                resource_timeout
            ));
            break;
        }
        waited_for_resources = true;
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if waited_for_resources {
        // Anything that arrived on the final pass still needs a restyle before
        // it can affect layout.
        document.resolve(0.0);
    }
    document.as_mut().resolve(0.0);

    // Full-page height comes from the computed root layout.
    let computed_height = document.as_ref().root_element().final_layout.size.height as f64;

    let css_height = if opts.full_page {
        let wanted = computed_height.max(opts.height as f64);
        let bounded = wanted.min(MAX_RENDER_HEIGHT);
        if bounded < wanted {
            notes.push(format!(
                "full-page height clamped from {wanted:.0} to {bounded:.0} CSS px \
                 (MAX_RENDER_HEIGHT); image is truncated"
            ));
        }
        bounded
    } else {
        opts.height as f64
    };

    let render_width = ((width as f64) * scale as f64).round() as u32;
    let render_height = (css_height * scale as f64).round().max(1.0) as u32;

    let total = render_width as u64 * render_height as u64;
    if total > MAX_RENDER_PIXELS {
        return Err(RenderError::TooLarge {
            width: render_width,
            height: render_height,
            max: MAX_RENDER_PIXELS,
        });
    }

    let background = opts.background;
    // Blitz/Vello are third-party code walking an untrusted document. Obscura's
    // stated anti-panic protocol is that a bad page degrades to an error rather
    // than aborting the process, and this crate is not exempt: a panic here
    // would otherwise unwind through the CDP dispatcher and take down every
    // other session sharing it.
    let buffer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| {
                scene.fill(
                    Fill::NonZero,
                    Default::default(),
                    background,
                    Default::default(),
                    &Rect::new(0.0, 0.0, render_width as f64, render_height as f64),
                );
                paint_scene(
                    scene,
                    document.as_mut(),
                    scale as f64,
                    render_width,
                    render_height,
                    0,
                    0,
                );
            },
            render_width,
            render_height,
        )
    }))
    .map_err(|e| {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string());
        RenderError::PaintPanic(msg)
    })?;

    // Crop after painting: Blitz paints the whole scene, and cropping the RGBA
    // buffer is both simpler and more faithful than trying to translate the
    // scene, which would change how fixed-position elements land.
    let (buffer, out_w, out_h) = match opts.clip {
        Some(clip) => crop_rgba(
            &buffer,
            render_width,
            render_height,
            clip,
            scale,
            &mut notes,
        ),
        None => (buffer, render_width, render_height),
    };

    let bytes = match opts.format {
        ImageFormat::Png => encode_png(&buffer, out_w, out_h)?,
        ImageFormat::Jpeg => encode_jpeg(&buffer, out_w, out_h, opts.jpeg_quality)?,
    };

    Ok(RenderOutcome {
        bytes,
        width: out_w,
        height: out_h,
        format: opts.format,
        fidelity_notes: notes,
    })
}

/// Crop an RGBA buffer to `clip` (CSS px, pre-scale), clamped to the buffer.
///
/// An out-of-bounds clip is clamped and noted rather than erroring: CDP clients
/// routinely compute a clip from a stale layout, and failing the whole capture
/// over a few pixels is worse than returning the overlap.
fn crop_rgba(
    buffer: &[u8],
    buf_w: u32,
    buf_h: u32,
    clip: Clip,
    scale: f32,
    notes: &mut Vec<String>,
) -> (Vec<u8>, u32, u32) {
    let s = scale as f64;
    let x0 = (clip.x * s).round().max(0.0) as u32;
    let y0 = (clip.y * s).round().max(0.0) as u32;
    let w = (clip.width * s).round().max(1.0) as u32;
    let h = (clip.height * s).round().max(1.0) as u32;

    let x1 = (x0 + w).min(buf_w);
    let y1 = (y0 + h).min(buf_h);

    if x0 >= buf_w || y0 >= buf_h {
        notes.push(format!(
            "clip origin ({x0},{y0}) lies outside the {buf_w}x{buf_h} render; \
             returning the uncropped image"
        ));
        return (buffer.to_vec(), buf_w, buf_h);
    }
    if x1 - x0 != w || y1 - y0 != h {
        notes.push(format!(
            "clip {w}x{h} at ({x0},{y0}) exceeded the {buf_w}x{buf_h} render and was clamped"
        ));
    }

    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut out = Vec::with_capacity((cw as usize) * (ch as usize) * 4);
    for row in y0..y1 {
        let start = ((row * buf_w + x0) as usize) * 4;
        let end = start + (cw as usize) * 4;
        out.extend_from_slice(&buffer[start..end]);
    }
    (out, cw, ch)
}

fn encode_png(buffer: &[u8], width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| RenderError::Encode(e.to_string()))?;
        writer
            .write_image_data(buffer)
            .map_err(|e| RenderError::Encode(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| RenderError::Encode(e.to_string()))?;
    }
    Ok(out)
}

fn encode_jpeg(
    buffer: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    // JPEG has no alpha; the caller already composited onto an opaque
    // background, so dropping the alpha channel here is lossless in practice.
    encoder
        .encode(
            buffer,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| RenderError::Encode(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No test document here has external subresources, so the budget is only
    /// a backstop and can be short.
    const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    const DOC: &str = r#"<!doctype html><html><body style="margin:0">
        <div style="width:100px;height:50px;background:#ff0000"></div>
        </body></html>"#;

    fn opts() -> RenderOptions {
        RenderOptions {
            width: 200,
            height: 100,
            ..Default::default()
        }
    }

    #[test]
    fn renders_png_at_requested_size() {
        let out = render_html(DOC, None, &opts(), TEST_TIMEOUT).expect("render");
        assert_eq!((out.width, out.height), (200, 100));
        assert_eq!(out.format, ImageFormat::Png);
        // PNG magic — proves we returned an actual encoded image, not a buffer.
        assert_eq!(
            &out.bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
    }

    #[test]
    fn renders_jpeg_when_asked() {
        let o = RenderOptions {
            format: ImageFormat::Jpeg,
            ..opts()
        };
        let out = render_html(DOC, None, &o, TEST_TIMEOUT).expect("render");
        assert_eq!(out.format, ImageFormat::Jpeg);
        assert_eq!(&out.bytes[..2], &[0xff, 0xd8]); // SOI
    }

    #[test]
    fn scale_multiplies_output_pixels() {
        let o = RenderOptions {
            scale: 2.0,
            ..opts()
        };
        let out = render_html(DOC, None, &o, TEST_TIMEOUT).expect("render");
        assert_eq!((out.width, out.height), (400, 200));
    }

    /// The regression that matters most: a screenshot of a red box must contain
    /// red pixels. A renderer that lays out correctly but paints nothing still
    /// returns a valid, correctly-sized PNG — so size assertions alone cannot
    /// tell "working" from "blank", which is exactly how a broken paint path
    /// ships green.
    #[test]
    fn actually_paints_content_not_a_blank_image() {
        let o = RenderOptions {
            format: ImageFormat::Png,
            ..opts()
        };
        let out = render_html(DOC, None, &o, TEST_TIMEOUT).expect("render");
        let decoder = png::Decoder::new(out.bytes.as_slice());
        let mut reader = decoder.read_info().expect("png header");
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("png frame");
        let px = &buf[..info.buffer_size()];

        let reddish = px
            .chunks_exact(4)
            .filter(|c| c[0] > 200 && c[1] < 80 && c[2] < 80)
            .count();
        assert!(
            reddish > 1000,
            "expected the 100x50 red box to paint ~5000 red pixels, found {reddish}"
        );
    }

    /// Regression for the bug that shipped and was only caught by rendering a
    /// real site: the subresource budget used to be a *pass count*, and 32
    /// passes elapse in well under one network round-trip, so any page with an
    /// external stylesheet rendered unstyled while the budget looked generous.
    ///
    /// The budget is wall-clock now, and this asserts the property that makes
    /// it correct: a render that has to wait actually spends real time waiting,
    /// rather than burning the budget in a tight spin.
    #[tokio::test]
    async fn external_subresource_wait_is_wall_clock_not_pass_count() {
        // Points at a port nothing is listening on, so the stylesheet fetch can
        // never complete and the budget must be spent in full.
        let doc = r#"<!doctype html><html><head>
            <link rel="stylesheet" href="http://127.0.0.1:9/never.css">
            </head><body>hi</body></html>"#;
        let budget = std::time::Duration::from_millis(300);

        let start = std::time::Instant::now();
        let out = render_html(doc, Some("http://127.0.0.1:9/"), &opts(), budget).expect("render");
        let elapsed = start.elapsed();

        assert!(
            elapsed >= budget,
            "an unreachable subresource must consume the wall-clock budget, \
             but the render returned after {elapsed:?} (budget {budget:?}) — \
             the spin-without-waiting bug is back"
        );
        assert!(
            out.fidelity_notes
                .iter()
                .any(|n| n.contains("still in flight")),
            "a render missing its stylesheet must say so: {:?}",
            out.fidelity_notes
        );
    }

    #[test]
    fn rejects_degenerate_viewport() {
        let o = RenderOptions { width: 0, ..opts() };
        assert!(matches!(
            render_html(DOC, None, &o, TEST_TIMEOUT),
            Err(RenderError::BadViewport(_))
        ));
    }

    #[test]
    fn rejects_oversized_render_instead_of_ooming() {
        let o = RenderOptions {
            width: 8000,
            height: 15000,
            scale: 4.0,
            ..Default::default()
        };
        assert!(matches!(
            render_html(DOC, None, &o, TEST_TIMEOUT),
            Err(RenderError::TooLarge { .. })
        ));
    }

    #[test]
    fn full_page_height_is_clamped_and_reported() {
        let tall = format!(
            r#"<!doctype html><body style="margin:0"><div style="height:{}px"></div></body>"#,
            MAX_RENDER_HEIGHT as u32 + 5000
        );
        let o = RenderOptions {
            full_page: true,
            ..opts()
        };
        let out = render_html(&tall, None, &o, TEST_TIMEOUT).expect("render");
        assert_eq!(out.height, MAX_RENDER_HEIGHT as u32);
        assert!(
            out.fidelity_notes.iter().any(|n| n.contains("clamped")),
            "a truncated screenshot must say so: {:?}",
            out.fidelity_notes
        );
    }

    #[test]
    fn clip_crops_the_output() {
        let o = RenderOptions {
            clip: Some(Clip {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 25.0,
            }),
            ..opts()
        };
        let out = render_html(DOC, None, &o, TEST_TIMEOUT).expect("render");
        assert_eq!((out.width, out.height), (50, 25));
    }

    #[test]
    fn out_of_bounds_clip_degrades_instead_of_failing() {
        let o = RenderOptions {
            clip: Some(Clip {
                x: 9999.0,
                y: 9999.0,
                width: 10.0,
                height: 10.0,
            }),
            ..opts()
        };
        let out = render_html(DOC, None, &o, TEST_TIMEOUT).expect("render");
        assert!(out.fidelity_notes.iter().any(|n| n.contains("outside")));
    }

    #[test]
    fn cdp_format_parsing_rejects_unencodable_formats() {
        assert_eq!(ImageFormat::from_cdp(None), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_cdp(Some("jpeg")), Some(ImageFormat::Jpeg));
        // webp is a real CDP format we cannot encode; silently returning PNG
        // would misreport the payload's type to the client.
        assert_eq!(ImageFormat::from_cdp(Some("webp")), None);
    }
}
