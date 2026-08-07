//! `Emulation` domain — currently just enough of it to make the render
//! viewport controllable.
//!
//! The whole domain used to be accepted-and-discarded in the dispatcher. That
//! was defensible while Obscura produced no pixels: nothing downstream read a
//! viewport, so recording one would have been dead state. Now that
//! `Page.captureScreenshot` renders, `setDeviceMetricsOverride` is load-bearing
//! — it is what `page.setViewport()` (Puppeteer) and `browser.newContext({
//! viewport })` (Playwright) call — and discarding it would silently render
//! every capture at the 1280x720 default.
//!
//! Methods other than the device-metrics pair keep the old no-op behaviour, so
//! the Puppeteer/Playwright connect path is unaffected.

use serde_json::{json, Value};

use crate::dispatch::{CdpContext, Viewport};

/// Guard rails on an emulated viewport.
///
/// A client can ask for anything; the renderer allocates width * height * 4
/// bytes, so a nonsense value has to be rejected here rather than at malloc.
/// These bounds are deliberately generous — they exist to stop absurdity, not
/// to enforce a policy.
const MIN_DIM: u32 = 1;
const MAX_DIM: u32 = 16_384;
const MAX_SCALE: f32 = 8.0;

pub async fn handle(method: &str, params: &Value, ctx: &mut CdpContext) -> Result<Value, String> {
    match method {
        "setDeviceMetricsOverride" => {
            let width = params
                .get("width")
                .and_then(|v| v.as_u64())
                .ok_or("setDeviceMetricsOverride: width required")? as u32;
            let height = params
                .get("height")
                .and_then(|v| v.as_u64())
                .ok_or("setDeviceMetricsOverride: height required")?
                as u32;

            // CDP says width/height of 0 means "override with the current
            // value", i.e. do not change that dimension.
            let width = if width == 0 {
                ctx.viewport.width
            } else {
                width
            };
            let height = if height == 0 {
                ctx.viewport.height
            } else {
                height
            };

            // Likewise deviceScaleFactor 0 means "use the default", which for
            // a headless renderer is 1.0 rather than a physical display's DPR.
            let scale = params
                .get("deviceScaleFactor")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32)
                .filter(|f| f.is_finite() && *f > 0.0)
                .unwrap_or(1.0);

            if !(MIN_DIM..=MAX_DIM).contains(&width) || !(MIN_DIM..=MAX_DIM).contains(&height) {
                return Err(format!(
                    "setDeviceMetricsOverride: {width}x{height} outside the supported \
                     {MIN_DIM}..={MAX_DIM} range"
                ));
            }
            if scale > MAX_SCALE {
                return Err(format!(
                    "setDeviceMetricsOverride: deviceScaleFactor {scale} exceeds {MAX_SCALE}"
                ));
            }

            ctx.viewport = Viewport {
                width,
                height,
                scale,
            };
            Ok(json!({}))
        }
        "clearDeviceMetricsOverride" => {
            ctx.viewport = Viewport::default();
            Ok(json!({}))
        }
        // Unimplemented but harmless: returning an error here would break the
        // Puppeteer/Playwright connect path, which fires several Emulation
        // setters unconditionally.
        _ => Ok(json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CdpContext {
        CdpContext::new()
    }

    #[tokio::test]
    async fn sets_viewport_that_screenshots_will_use() {
        let mut c = ctx();
        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 390, "height": 844, "deviceScaleFactor": 3.0, "mobile": true}),
            &mut c,
        )
        .await
        .expect("override accepted");
        assert_eq!(
            c.viewport,
            Viewport {
                width: 390,
                height: 844,
                scale: 3.0
            }
        );
    }

    /// The regression this whole module exists to prevent: `Emulation` was a
    /// blanket no-op, so a client could set a mobile viewport, get `{}` back,
    /// and receive a 1280px desktop screenshot with no indication anything was
    /// ignored.
    #[tokio::test]
    async fn override_is_not_silently_discarded() {
        let mut c = ctx();
        let before = c.viewport;
        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 800, "height": 600}),
            &mut c,
        )
        .await
        .unwrap();
        assert_ne!(c.viewport, before, "viewport must actually change");
        assert_eq!(c.viewport.width, 800);
    }

    #[tokio::test]
    async fn zero_dimension_means_keep_current_per_cdp() {
        let mut c = ctx();
        c.viewport = Viewport {
            width: 1000,
            height: 500,
            scale: 1.0,
        };
        handle(
            "setDeviceMetricsOverride",
            &json!({"width": 0, "height": 0}),
            &mut c,
        )
        .await
        .unwrap();
        assert_eq!((c.viewport.width, c.viewport.height), (1000, 500));
    }

    #[tokio::test]
    async fn clear_restores_the_default() {
        let mut c = ctx();
        c.viewport = Viewport {
            width: 390,
            height: 844,
            scale: 3.0,
        };
        handle("clearDeviceMetricsOverride", &json!({}), &mut c)
            .await
            .unwrap();
        assert_eq!(c.viewport, Viewport::default());
    }

    #[tokio::test]
    async fn rejects_absurd_dimensions_rather_than_attempting_the_allocation() {
        let mut c = ctx();
        let err = handle(
            "setDeviceMetricsOverride",
            &json!({"width": 100_000, "height": 100_000}),
            &mut c,
        )
        .await
        .expect_err("must reject");
        assert!(err.contains("outside the supported"), "{err}");
        assert_eq!(c.viewport, Viewport::default(), "must not partially apply");
    }

    /// Other Emulation methods must keep succeeding — Puppeteer and Playwright
    /// call several of them during connect and treat an error as fatal.
    #[tokio::test]
    async fn other_emulation_methods_still_no_op_successfully() {
        let mut c = ctx();
        for m in [
            "setUserAgentOverride",
            "setTouchEmulationEnabled",
            "setEmulatedMedia",
            "setGeolocationOverride",
        ] {
            assert!(
                handle(m, &json!({}), &mut c).await.is_ok(),
                "{m} must not error"
            );
        }
    }
}
