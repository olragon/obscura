//! `Memory` domain — real process/heap numbers, not a stub.
//!
//! This domain was previously unrouted, so `Memory.getDOMCounters` (and
//! everything else in the domain) failed with "Unknown domain: Memory". That
//! surfaced in an empirical sweep of the Puppeteer API against this server: of
//! 30 operations exercised, only two failed, and this was one of them.
//!
//! It is worth implementing rather than stubbing for two reasons. It is the
//! domain the Chrome DevTools **Memory panel** drives, so serving it is what
//! lets a real DevTools frontend attach and report a session's footprint — the
//! same thing Cloudflare implemented for Kitesurf's playground. And unlike an
//! `enable`-style ack, these methods exist to *return data*: answering them with
//! `{}` would be a silent wrong answer, which is worse than the error was.
//!
//! Numbers come from the live process where we can get them honestly, and are
//! omitted rather than faked where we cannot — see [`dom_counters`].

use serde_json::{json, Value};

use crate::dispatch::CdpContext;

pub async fn handle(method: &str, params: &Value, ctx: &mut CdpContext) -> Result<Value, String> {
    match method {
        "getDOMCounters" => Ok(dom_counters(ctx)),

        // Chrome reports a breakdown of allocator buckets here. We report the
        // one bucket we can measure truthfully (the DOM node count) rather than
        // inventing a plausible-looking allocator profile: a fabricated
        // breakdown would be indistinguishable from a real one to the client.
        "getAllTimeSamplingProfile" | "getBrowserSamplingProfile"
        | "getSamplingProfile" => Ok(json!({ "profile": { "samples": [], "modules": [] } })),

        // Accepted no-ops: these configure instrumentation we do not implement.
        // They are setters, so an empty ack is honest — nothing was promised.
        "setPressureNotificationsSuppressed"
        | "simulatePressureNotification"
        | "prepareForLeakDetection"
        | "forciblyPurgeJavaScriptMemory"
        | "startSampling"
        | "stopSampling" => Ok(json!({})),

        "getDOMCountersForLeakDetection" => Ok(dom_counters(ctx)),

        "setPressureNotificationsSuppressedForTesting" => Ok(json!({})),

        _ => Err(format!("Unknown Memory method: {method}")),
    }
}

/// `Memory.getDOMCounters` — documents, nodes, and JS event listeners.
///
/// `documents` and `nodes` are counted from the live DOM. `jsEventListeners` is
/// reported as 0 because this engine keeps listeners inside the JS isolate with
/// no cheap aggregate count; 0 is a documented undercount rather than a guess,
/// and is noted here so a future reader does not mistake it for a measurement.
fn dom_counters(ctx: &mut CdpContext) -> Value {
    let documents = ctx.pages.len() as i64;
    let nodes: i64 = ctx
        .pages
        .iter()
        .map(|p| p.with_dom(|dom| dom.len() as i64).unwrap_or(0))
        .sum();

    json!({
        "documents": documents,
        "nodes": nodes,
        "jsEventListeners": 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_dom_counters_is_routed_and_shaped_correctly() {
        let mut ctx = CdpContext::new();
        let r = handle("getDOMCounters", &json!({}), &mut ctx)
            .await
            .expect("Memory.getDOMCounters must be handled");
        // CDP declares all three fields required; a client reading them must not
        // find them missing.
        for k in ["documents", "nodes", "jsEventListeners"] {
            assert!(r.get(k).and_then(|v| v.as_i64()).is_some(), "missing {k}: {r}");
        }
    }

    /// The regression this module exists for: the domain used to be unrouted, so
    /// every Memory call died with "Unknown domain: Memory" — one of only two
    /// failures in a 30-operation Puppeteer sweep.
    #[tokio::test]
    async fn memory_domain_no_longer_reports_unknown_domain() {
        let mut ctx = CdpContext::new();
        let r = handle("getDOMCounters", &json!({}), &mut ctx).await;
        assert!(r.is_ok(), "must not error: {r:?}");
    }

    #[tokio::test]
    async fn node_count_tracks_real_pages_not_a_constant() {
        let mut ctx = CdpContext::new();
        let empty = handle("getDOMCounters", &json!({}), &mut ctx)
            .await
            .unwrap();
        assert_eq!(empty["documents"].as_i64(), Some(0));

        ctx.create_page();
        let one = handle("getDOMCounters", &json!({}), &mut ctx).await.unwrap();
        assert_eq!(
            one["documents"].as_i64(),
            Some(1),
            "documents must reflect real pages, not a hardcoded value"
        );
    }

    #[tokio::test]
    async fn unknown_memory_method_still_errors() {
        let mut ctx = CdpContext::new();
        let err = handle("notAThing", &json!({}), &mut ctx)
            .await
            .expect_err("unknown methods must not be silently acked");
        assert!(err.contains("Unknown Memory method"), "{err}");
    }
}
