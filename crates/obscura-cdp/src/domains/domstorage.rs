//! `DOMStorage` domain — read/write `localStorage` and `sessionStorage` over
//! the wire.
//!
//! This is the surface DevTools' Application panel drives, and the one a CDP
//! client needs to save or restore a session *without* injecting script into
//! the page. Playwright itself reads storage by evaluating JS in each frame,
//! which works but requires a live document at the right origin; these methods
//! address the jar directly, so an agent can seed a login before the first
//! navigation.
//!
//! `storageId.securityOrigin` (or `storageKey`) is client-supplied here — that
//! is the protocol's contract, and a CDP client is already fully privileged.
//! Page script cannot reach this path; the ops in `obscura-js` derive the
//! origin in Rust precisely because *that* caller is untrusted.

use serde_json::{json, Value};

use crate::dispatch::CdpContext;
use obscura_net::{StorageArea, StorageJar};

fn jar_for(ctx: &CdpContext, session_id: &Option<String>) -> std::sync::Arc<StorageJar> {
    ctx.get_session_page(session_id)
        .map(|page| page.context.storage.clone())
        .unwrap_or_else(|| ctx.default_context.storage.clone())
}

/// Pull `(origin, area)` out of a `StorageId`. Chrome accepts either
/// `securityOrigin` or the newer `storageKey`; we normalize both through the
/// same origin parser the JS ops use, so `https://x.com/` and `https://x.com`
/// address one area rather than two.
fn storage_id(params: &Value) -> Result<(String, StorageArea), String> {
    let id = params
        .get("storageId")
        .ok_or_else(|| "Missing storageId".to_string())?;
    let raw = id
        .get("securityOrigin")
        .or_else(|| id.get("storageKey"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "storageId needs securityOrigin or storageKey".to_string())?;
    let origin = obscura_net::origin_of(raw)
        .ok_or_else(|| format!("Opaque or unsupported origin: {raw}"))?;
    let area = if id
        .get("isLocalStorage")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
    {
        StorageArea::Local
    } else {
        StorageArea::Session
    };
    Ok((origin, area))
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        // No events are emitted for storage mutations yet, so enable/disable are
        // honest acks: nothing was promised beyond accepting the call.
        "enable" | "disable" => Ok(json!({})),

        "getDOMStorageItems" => {
            let (origin, area) = storage_id(params)?;
            let entries: Vec<[String; 2]> = jar_for(ctx, session_id)
                .items(&origin, area)
                .into_iter()
                .map(|(k, v)| [k, v])
                .collect();
            Ok(json!({ "entries": entries }))
        }

        "setDOMStorageItem" => {
            let (origin, area) = storage_id(params)?;
            let key = params
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing key".to_string())?;
            let value = params
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing value".to_string())?;
            jar_for(ctx, session_id)
                .set_item(&origin, area, key, value)
                .map_err(|_| "QuotaExceededError".to_string())?;
            Ok(json!({}))
        }

        "removeDOMStorageItem" => {
            let (origin, area) = storage_id(params)?;
            let key = params
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing key".to_string())?;
            jar_for(ctx, session_id).remove_item(&origin, area, key);
            Ok(json!({}))
        }

        "clear" => {
            let (origin, area) = storage_id(params)?;
            jar_for(ctx, session_id).clear(&origin, area);
            Ok(json!({}))
        }

        _ => Err(format!("Unknown DOMStorage method: {method}")),
    }
}
