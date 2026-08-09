use serde_json::{json, Value};

use crate::cookie_params::{parse_cdp_cookie, parse_delete_cookies_params};
use crate::dispatch::CdpContext;
use crate::domains::network::cookie_info_to_cdp_json;

fn cookie_jar_for(
    ctx: &CdpContext,
    params: &Value,
    session_id: &Option<String>,
) -> Result<std::sync::Arc<obscura_net::CookieJar>, String> {
    match params.get("browserContextId").and_then(|value| value.as_str()) {
        Some(id) => ctx
            .browser_context(id)
            .map(|context| context.cookie_jar.clone())
            .ok_or_else(|| format!("Browser context not found: {}", id)),
        None => Ok(ctx
            .get_session_page(session_id)
            .map(|page| page.context.cookie_jar.clone())
            .unwrap_or_else(|| ctx.default_context.cookie_jar.clone())),
    }
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "getCookies" => {
            let cookies = cookie_jar_for(ctx, params, session_id)?.get_all_cookies();
            let cdp_cookies: Vec<Value> = cookies.iter().map(cookie_info_to_cdp_json).collect();
            Ok(json!({ "cookies": cdp_cookies }))
        }
        "setCookies" => {
            if let Some(cookies) = params.get("cookies").and_then(|v| v.as_array()) {
                let parsed: Vec<_> = cookies.iter().filter_map(parse_cdp_cookie).collect();
                cookie_jar_for(ctx, params, session_id)?.set_cookies_from_cdp(parsed);
            }
            Ok(json!({}))
        }
        "deleteCookies" => {
            if let Some(filter) = parse_delete_cookies_params(params) {
                cookie_jar_for(ctx, params, session_id)?.delete_cookies_filtered(
                    &filter.name,
                    &filter.domain,
                    filter.path.as_deref(),
                );
            }
            Ok(json!({}))
        }
        // `clearDataForOrigin` / `clearDataForStorageKey` take an explicit list
        // of storage types precisely because "the data for an origin" is no
        // longer one thing. We honour the two we actually hold (cookies, web
        // storage) and ignore the rest rather than pretending to clear an
        // IndexedDB we never implemented.
        "clearDataForOrigin" | "clearDataForStorageKey" => {
            let raw_origin = params
                .get("origin")
                .or_else(|| params.get("storageKey"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let types = params
                .get("storageTypes")
                .and_then(|v| v.as_str())
                .unwrap_or("all");
            let wants = |t: &str| types == "all" || types.split(',').any(|s| s.trim() == t);

            if let Some(origin) = obscura_net::origin_of(raw_origin) {
                let jar = ctx
                    .get_session_page(session_id)
                    .map(|page| page.context.storage.clone())
                    .unwrap_or_else(|| ctx.default_context.storage.clone());
                if types == "all" {
                    jar.clear_origin(&origin);
                } else if wants("local_storage") {
                    jar.clear(&origin, obscura_net::StorageArea::Local);
                }
            }

            if wants("cookies") {
                if let Ok(parsed) = url::Url::parse(raw_origin) {
                    if let Some(domain) = parsed.host_str() {
                        cookie_jar_for(ctx, params, session_id)?.clear_domain(domain);
                    }
                }
            }
            Ok(json!({}))
        }

        _ => Ok(json!({})),
    }
}
