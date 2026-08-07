//! End-to-end proof that `Page.captureScreenshot` renders the **post-JavaScript**
//! DOM to real pixels, driven through the same CDP dispatch a Puppeteer client
//! uses.
//!
//! The unit tests in `obscura-render` prove the renderer paints; these prove the
//! wiring. That distinction matters here: the renderer could be perfect and the
//! CDP arm could still hand it the pre-script HTML, the wrong viewport, or a
//! blank document, and every size-and-magic-byte assertion would still pass.
//! So each test below asserts on **decoded pixel content**, not on the envelope.

use base64::Engine as _;
use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves a page whose visible colour is decided by JavaScript at runtime.
///
/// The markup ships a BLUE box; a script then repaints it RED. A screenshot of
/// the parsed-but-unexecuted HTML would therefore be blue, and only a capture
/// of the post-script DOM is red — which is what makes "did we render after JS
/// ran?" an observable property rather than an assumption.
async fn serve_js_recolored_page(hits: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..hits {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let body = r#"<!doctype html><html><body style="margin:0">
<div id="box" style="width:200px;height:100px;background:#0000ff"></div>
<script>
  document.getElementById('box').style.background = '#ff0000';
</script>
</body></html>"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/")
}

async fn cdp(ctx: &mut CdpContext, id: u64, method: &str, params: Value, session: &str) -> Value {
    let resp = dispatch(
        &CdpRequest {
            id,
            method: method.to_string(),
            params,
            session_id: Some(session.to_string()),
        },
        ctx,
    )
    .await;
    assert!(
        resp.error.is_none(),
        "CDP {method} failed: {:?}",
        resp.error
    );
    resp.result.unwrap_or_else(|| json!({}))
}

/// Decode a base64 PNG from a CDP screenshot result into (width, height, rgba).
fn decode_png(result: &Value) -> (u32, u32, Vec<u8>) {
    let b64 = result["data"]
        .as_str()
        .expect("captureScreenshot must return a `data` string");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("data must be valid base64");
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "default format must be a real PNG"
    );
    let decoder = png::Decoder::new(bytes.as_slice());
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn count_matching(rgba: &[u8], pred: impl Fn(&[u8]) -> bool) -> usize {
    rgba.chunks_exact(4).filter(|c| pred(c)).count()
}

async fn setup(hits: usize) -> (CdpContext, String, String) {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    let url = serve_js_recolored_page(hits).await;
    let mut ctx = CdpContext::new();
    let page_id = ctx.create_page();
    let session = "session-1".to_string();
    ctx.sessions.insert(session.clone(), page_id);
    (ctx, url, session)
}

/// The headline: a screenshot contains the colour JavaScript applied, not the
/// colour in the served markup.
#[tokio::test(flavor = "current_thread")]
async fn screenshot_captures_the_post_javascript_dom() {
    let (mut ctx, url, session) = setup(1).await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url}), &session).await;

    let shot = cdp(&mut ctx, 2, "Page.captureScreenshot", json!({}), &session).await;
    let (w, h, rgba) = decode_png(&shot);
    assert_eq!((w, h), (1280, 720), "default viewport should be Chrome's");

    let red = count_matching(&rgba, |c| c[0] > 200 && c[1] < 80 && c[2] < 80);
    let blue = count_matching(&rgba, |c| c[2] > 200 && c[0] < 80 && c[1] < 80);

    assert!(
        red > 15_000,
        "the 200x100 box should paint ~20k red px after the script runs; got {red} red / {blue} blue"
    );
    assert_eq!(
        blue, 0,
        "blue pixels mean the pre-script markup was rendered, not the live DOM"
    );
}

/// `Emulation.setDeviceMetricsOverride` must actually change the rendered
/// image. Before this work the whole Emulation domain was an accept-and-discard
/// no-op, so this asserts against a regression that would otherwise be
/// completely silent — the client gets a valid PNG, just the wrong size.
#[tokio::test(flavor = "current_thread")]
async fn emulated_viewport_changes_the_rendered_size() {
    let (mut ctx, url, session) = setup(1).await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url}), &session).await;
    cdp(
        &mut ctx,
        2,
        "Emulation.setDeviceMetricsOverride",
        json!({"width": 400, "height": 300, "deviceScaleFactor": 2.0}),
        &session,
    )
    .await;

    let shot = cdp(&mut ctx, 3, "Page.captureScreenshot", json!({}), &session).await;
    let (w, h, _) = decode_png(&shot);
    assert_eq!(
        (w, h),
        (800, 600),
        "400x300 at DPR 2 must render 800x600 device px"
    );
}

/// `getLayoutMetrics` is what Playwright reads to size its capture, so it must
/// agree with the image the very next call produces. A mismatch here is the
/// classic silent mis-crop.
#[tokio::test(flavor = "current_thread")]
async fn layout_metrics_agree_with_the_rendered_image() {
    let (mut ctx, url, session) = setup(1).await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url}), &session).await;
    cdp(
        &mut ctx,
        2,
        "Emulation.setDeviceMetricsOverride",
        json!({"width": 640, "height": 480}),
        &session,
    )
    .await;

    let metrics = cdp(&mut ctx, 3, "Page.getLayoutMetrics", json!({}), &session).await;
    let shot = cdp(&mut ctx, 4, "Page.captureScreenshot", json!({}), &session).await;
    let (w, h, _) = decode_png(&shot);

    assert_eq!(
        metrics["layoutViewport"]["clientWidth"].as_f64(),
        Some(w as f64)
    );
    assert_eq!(
        metrics["layoutViewport"]["clientHeight"].as_f64(),
        Some(h as f64)
    );
}

/// JPEG output should be a JPEG. Trivial, but the format parameter is the one
/// place where returning the wrong bytes under the right key is undetectable
/// by a client that trusts the envelope.
#[tokio::test(flavor = "current_thread")]
async fn jpeg_format_returns_jpeg_bytes() {
    let (mut ctx, url, session) = setup(1).await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url}), &session).await;

    let shot = cdp(
        &mut ctx,
        2,
        "Page.captureScreenshot",
        json!({"format": "jpeg", "quality": 70}),
        &session,
    )
    .await;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(shot["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(&bytes[..2], &[0xff, 0xd8], "JPEG SOI marker");
}

/// A clip must crop the real image, not just be accepted and ignored.
#[tokio::test(flavor = "current_thread")]
async fn clip_crops_the_returned_image() {
    let (mut ctx, url, session) = setup(1).await;
    cdp(&mut ctx, 1, "Page.navigate", json!({"url": url}), &session).await;

    let shot = cdp(
        &mut ctx,
        2,
        "Page.captureScreenshot",
        json!({"clip": {"x": 0, "y": 0, "width": 120, "height": 60, "scale": 1}}),
        &session,
    )
    .await;
    let (w, h, rgba) = decode_png(&shot);
    assert_eq!((w, h), (120, 60));
    // The crop sits entirely inside the (repainted) box, so it should be solid red.
    let red = count_matching(&rgba, |c| c[0] > 200 && c[1] < 80 && c[2] < 80);
    assert!(
        red > (120 * 60) / 2,
        "cropped region should be mostly the red box, got {red} red px"
    );
}
