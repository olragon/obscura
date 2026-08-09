use std::path::PathBuf;
use std::sync::Arc;

use obscura_net::{CookieJar, ObscuraHttpClient, RobotsCache, StorageArea, StorageJar};

pub struct BrowserContext {
    pub id: String,
    pub cookie_jar: Arc<CookieJar>,
    /// Origin-keyed `localStorage`/`sessionStorage`, shared by every page in
    /// this context — the other half of a durable session. `localStorage` is
    /// restored from and written back to `{storage_dir}/storage.json`;
    /// `sessionStorage` stays in memory, as it does in a real browser.
    pub storage: Arc<StorageJar>,
    pub http_client: Arc<ObscuraHttpClient>,
    pub user_agent: String,
    pub platform: String,
    pub ua_platform: String,
    pub ua_platform_version: String,
    pub proxy_url: Option<String>,
    pub robots_cache: Arc<RobotsCache>,
    pub obey_robots: bool,
    pub stealth: bool,
    /// When true, CDP-driven navigation to file:// URLs is permitted.
    /// Default is false: a remote CDP client cannot point the browser
    /// at /etc/shadow even if Obscura is running as a privileged user.
    /// Flip on via `obscura serve --allow-file-access` for legitimate
    /// local-HTML testing workflows. The CLI's own `obscura fetch
    /// file://...` path is unaffected because it does not go through
    /// the CDP server.
    pub allow_file_access: bool,
    pub storage_dir: Option<PathBuf>,
    /// When true, the http client allows fetching localhost / RFC1918 /
    /// link-local addresses. Set via `--allow-private-network` (issue #33).
    /// Independent of `allow_file_access` because they cover different threat
    /// models: file:// is a local file-system read, while private-network is
    /// the broader SSRF gate from issue #4.
    pub allow_private_network: bool,
}

impl BrowserContext {
    pub fn new(id: String) -> Self {
        Self::_new_inner(id, None, false, None, None, false)
    }

    /// Create a BrowserContext with an optional storage directory.
    /// When `storage_dir` is set, cookies are automatically loaded from
    /// `{storage_dir}/cookies.json` on creation.
    pub fn with_storage(
        id: String,
        storage_dir: Option<PathBuf>,
    ) -> Self {
        Self::_new_inner(id, None, false, None, storage_dir, false)
    }

    /// Create a BrowserContext with full options including storage_dir.
    pub fn with_storage_full(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, storage_dir, false)
    }

    /// Variant that also accepts the `allow_private_network` opt-in. All
    /// pre-existing constructors default it to `false`; callers that want the
    /// CLI's `--allow-private-network` (issue #33) behaviour go through here.
    pub fn with_storage_and_network(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, storage_dir, allow_private_network)
    }

    fn _new_inner(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
        storage_dir: Option<PathBuf>,
        allow_private_network: bool,
    ) -> Self {
        let cookie_jar = Arc::new(CookieJar::new());
        let storage = Arc::new(StorageJar::new());

        // Restore cookies from disk if storage_dir is configured
        if let Some(ref dir) = storage_dir {
            let cookie_path = dir.join("cookies.json");
            if cookie_path.exists() {
                match cookie_jar.load_from_file(&cookie_path) {
                    Ok(n) if n > 0 => {
                        tracing::info!("Loaded {} cookies from {}", n, cookie_path.display());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("Failed to load cookies from {}: {}", cookie_path.display(), e);
                    }
                }
            }

            let storage_path = dir.join(obscura_net::STORAGE_FILE);
            match storage.load_from_file(&storage_path) {
                Ok(n) if n > 0 => {
                    tracing::info!("Loaded {} storage items from {}", n, storage_path.display());
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "Failed to load storage from {}: {}",
                        storage_path.display(),
                        e
                    );
                }
            }
        }

        let mut client = ObscuraHttpClient::with_full_options(
            cookie_jar.clone(),
            proxy_url.as_deref(),
            allow_private_network,
        );
        if stealth {
            client.block_trackers = true;
        }
        let profile = crate::profiles::select_profile();
        let resolved_ua = user_agent.unwrap_or_else(|| profile.user_agent.to_string());
        let platform = profile.platform.to_string();
        let ua_platform = profile.ua_platform.to_string();
        let ua_platform_version = profile.ua_platform_version.to_string();
        // Sync the http client's UA at construction so navigation requests pick it
        // up before any async setup runs. The lock has no other holders here, so
        // try_write always succeeds; we fall back silently if it ever fails.
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = resolved_ua.clone();
        }
        let http_client = Arc::new(client);
        BrowserContext {
            id,
            cookie_jar,
            storage,
            http_client,
            user_agent: resolved_ua,
            platform,
            ua_platform,
            ua_platform_version,
            proxy_url,
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: false,
            stealth,
            allow_file_access: false,
            storage_dir,
            allow_private_network,
        }
    }

    pub fn with_options(id: String, proxy_url: Option<String>, stealth: bool) -> Self {
        Self::with_full_options(id, proxy_url, stealth, None)
    }

    pub fn with_full_options(
        id: String,
        proxy_url: Option<String>,
        stealth: bool,
        user_agent: Option<String>,
    ) -> Self {
        Self::_new_inner(id, proxy_url, stealth, user_agent, None, false)
    }

    pub fn with_proxy(id: String, proxy_url: Option<String>) -> Self {
        Self::with_options(id, proxy_url, false)
    }

    /// Create a context with the same browser configuration but independent
    /// mutable network state. Persistent copies start with the template's
    /// current cookies; incognito copies start empty and never write to the
    /// template's storage directory.
    pub fn isolated_copy(&self, id: String, persistent: bool) -> Self {
        let cookie_jar = Arc::new(CookieJar::new());
        let storage = Arc::new(StorageJar::new());
        if persistent {
            cookie_jar.set_cookies_from_cdp(self.cookie_jar.get_all_cookies());
            storage.copy_from(&self.storage);
        }

        let mut client = ObscuraHttpClient::with_full_options(
            cookie_jar.clone(),
            self.proxy_url.as_deref(),
            self.allow_private_network,
        );
        if self.stealth {
            client.block_trackers = true;
        }
        if let Ok(mut guard) = client.user_agent.try_write() {
            *guard = self.user_agent.clone();
        }

        BrowserContext {
            id,
            cookie_jar,
            storage,
            http_client: Arc::new(client),
            user_agent: self.user_agent.clone(),
            platform: self.platform.clone(),
            ua_platform: self.ua_platform.clone(),
            ua_platform_version: self.ua_platform_version.clone(),
            proxy_url: self.proxy_url.clone(),
            robots_cache: Arc::new(RobotsCache::new()),
            obey_robots: self.obey_robots,
            stealth: self.stealth,
            allow_file_access: self.allow_file_access,
            storage_dir: persistent.then(|| self.storage_dir.clone()).flatten(),
            allow_private_network: self.allow_private_network,
        }
    }

    /// Persist cookies to disk if storage_dir is configured.
    /// Called during graceful shutdown.
    pub fn save_cookies(&self) {
        if let Some(ref dir) = self.storage_dir {
            let _ = std::fs::create_dir_all(dir);
            let cookie_path = dir.join("cookies.json");
            if let Err(e) = self.cookie_jar.save_to_file(&cookie_path) {
                tracing::warn!("Failed to save cookies to {}: {}", cookie_path.display(), e);
            } else {
                tracing::info!("Saved cookies to {}", cookie_path.display());
            }
            self.save_storage();
        }
    }

    /// Persist `localStorage` to `{storage_dir}/storage.json` if configured.
    /// Written atomically (temp file + rename) because several pages in this
    /// context share the jar and a torn file silently poisons every later
    /// session that loads it.
    pub fn save_storage(&self) {
        if let Some(ref dir) = self.storage_dir {
            let _ = std::fs::create_dir_all(dir);
            let path = dir.join(obscura_net::STORAGE_FILE);
            if let Err(e) = self.storage.save_to_file(&path) {
                tracing::warn!("Failed to save storage to {}: {}", path.display(), e);
            } else {
                tracing::info!("Saved storage to {}", path.display());
            }
        }
    }

    /// Export a Playwright-shaped `storageState`:
    /// `{cookies: [...], origins: [{origin, localStorage: [{name, value}]}]}`.
    ///
    /// The shape is Playwright's on purpose — it is what every agent framework
    /// and CI fixture already speaks, so a session captured there imports here
    /// and vice versa. `sessionStorage` is included as an extra key per origin
    /// (Playwright omits it; an extra key is ignored by consumers that don't
    /// want it, and losing tab-scoped tokens silently would be worse).
    ///
    /// Known gaps, stated rather than implied: no IndexedDB, no Service Worker
    /// or Cache Storage state. Playwright does not capture those either.
    pub fn storage_state(&self) -> serde_json::Value {
        use serde_json::json;
        let cookies: Vec<serde_json::Value> = self
            .cookie_jar
            .get_all_cookies()
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "value": c.value,
                    "domain": c.domain,
                    "path": c.path,
                    "expires": c.expires.unwrap_or(-1),
                    "httpOnly": c.http_only,
                    "secure": c.secure,
                    "sameSite": normalize_same_site(&c.same_site),
                })
            })
            .collect();

        let origins: Vec<serde_json::Value> = self
            .storage
            .origins()
            .into_iter()
            .map(|origin| {
                let to_items = |items: Vec<(String, String)>| -> Vec<serde_json::Value> {
                    items
                        .into_iter()
                        .map(|(name, value)| json!({ "name": name, "value": value }))
                        .collect()
                };
                let local = to_items(self.storage.items(&origin, StorageArea::Local));
                let session = to_items(self.storage.items(&origin, StorageArea::Session));
                let mut entry = json!({ "origin": origin, "localStorage": local });
                if !session.is_empty() {
                    entry["sessionStorage"] = serde_json::Value::Array(session);
                }
                entry
            })
            .collect();

        json!({ "cookies": cookies, "origins": origins })
    }

    /// Import a `storageState`. Returns (cookies applied, storage items
    /// applied). Accepts both Playwright's `[{name, value}]` item shape and the
    /// `[[key, value]]` pair shape this project emitted previously, so old
    /// saved sessions keep working.
    pub fn set_storage_state(&self, state: &serde_json::Value) -> (usize, usize) {
        let mut cookies_applied = 0usize;
        let mut items_applied = 0usize;

        if let Some(cookies) = state.get("cookies").and_then(|v| v.as_array()) {
            let parsed: Vec<obscura_net::CookieInfo> = cookies
                .iter()
                .filter_map(|c| {
                    Some(obscura_net::CookieInfo {
                        name: c.get("name")?.as_str()?.to_string(),
                        value: c.get("value")?.as_str()?.to_string(),
                        domain: c.get("domain")?.as_str()?.to_string(),
                        path: c.get("path").and_then(|v| v.as_str()).unwrap_or("/").to_string(),
                        secure: c.get("secure").and_then(|v| v.as_bool()).unwrap_or(false),
                        http_only: c
                            .get("httpOnly")
                            .or_else(|| c.get("http_only"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        same_site: c
                            .get("sameSite")
                            .or_else(|| c.get("same_site"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        expires: c
                            .get("expires")
                            .and_then(|v| v.as_i64())
                            .filter(|e| *e > 0),
                    })
                })
                .collect();
            cookies_applied = parsed.len();
            self.cookie_jar.set_cookies_from_cdp(parsed);
        }

        if let Some(origins) = state.get("origins").and_then(|v| v.as_array()) {
            for entry in origins {
                let Some(origin) = entry.get("origin").and_then(|v| v.as_str()) else {
                    continue;
                };
                // Normalize through the same parser the ops use, so an entry
                // written as "https://x.com/" lands on the key a page at
                // https://x.com/path will read.
                let Some(origin) = obscura_net::origin_of(origin) else {
                    continue;
                };
                for (key, area) in [
                    ("localStorage", StorageArea::Local),
                    ("sessionStorage", StorageArea::Session),
                ] {
                    if let Some(arr) = entry.get(key).and_then(|v| v.as_array()) {
                        let items = parse_storage_items(arr);
                        items_applied += items.len();
                        self.storage.replace(&origin, area, items);
                    }
                }
            }
        }

        (cookies_applied, items_applied)
    }
}

/// Accept Playwright's `{name, value}` objects and this project's older
/// `[key, value]` pairs.
fn parse_storage_items(arr: &[serde_json::Value]) -> Vec<(String, String)> {
    arr.iter()
        .filter_map(|item| {
            if let Some(pair) = item.as_array() {
                Some((
                    pair.first()?.as_str()?.to_string(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            } else {
                Some((
                    item.get("name")?.as_str()?.to_string(),
                    item.get("value")?.as_str()?.to_string(),
                ))
            }
        })
        .collect()
}

/// Playwright serializes sameSite as `Strict` / `Lax` / `None`.
fn normalize_same_site(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "strict" => "Strict",
        "none" => "None",
        _ => "Lax",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn with_full_options_propagates_user_agent_to_http_client() {
        let ctx = BrowserContext::with_full_options(
            "test".to_string(),
            None,
            false,
            Some("Custom-UA/1.0".to_string()),
        );
        assert_eq!(ctx.user_agent, "Custom-UA/1.0");
        let client_ua = ctx.http_client.user_agent.read().await.clone();
        assert_eq!(client_ua, "Custom-UA/1.0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_full_options_falls_back_to_chrome_default() {
        let ctx = BrowserContext::with_full_options(
            "test".to_string(),
            None,
            false,
            None,
        );
        assert!(ctx.user_agent.contains("Chrome"));
        let client_ua = ctx.http_client.user_agent.read().await.clone();
        assert!(client_ua.contains("Chrome"));
        assert_eq!(ctx.user_agent, client_ua);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_options_keeps_default_user_agent() {
        let ctx = BrowserContext::with_options("test".to_string(), None, false);
        assert!(ctx.user_agent.contains("Chrome"));
    }

    #[test]
    fn storage_state_round_trips_through_playwright_shape() {
        let source = BrowserContext::new("source".to_string());
        source
            .cookie_jar
            .set_cookie("sid=abc; Path=/", &url::Url::parse("https://app.example").unwrap());
        source
            .storage
            .set_item("https://app.example", StorageArea::Local, "token", "jwt")
            .unwrap();
        source
            .storage
            .set_item("https://app.example", StorageArea::Session, "csrf", "n1")
            .unwrap();

        let state = source.storage_state();
        let origin = &state["origins"][0];
        assert_eq!(origin["origin"], "https://app.example");
        assert_eq!(origin["localStorage"][0]["name"], "token");
        assert_eq!(origin["localStorage"][0]["value"], "jwt");
        assert_eq!(origin["sessionStorage"][0]["name"], "csrf");
        // Playwright's cookie keys are camelCase; a snake_case export would
        // silently import as all-defaults in any other tool.
        assert_eq!(state["cookies"][0]["name"], "sid");
        assert!(state["cookies"][0].get("httpOnly").is_some());
        assert!(state["cookies"][0].get("sameSite").is_some());

        let target = BrowserContext::new("target".to_string());
        let (cookies, items) = target.set_storage_state(&state);
        assert_eq!((cookies, items), (1, 2));
        assert_eq!(
            target
                .storage
                .get_item("https://app.example", StorageArea::Local, "token")
                .as_deref(),
            Some("jwt")
        );
        assert_eq!(target.cookie_jar.get_all_cookies().len(), 1);
    }

    #[test]
    fn set_storage_state_accepts_the_legacy_pair_shape() {
        let ctx = BrowserContext::new("legacy".to_string());
        let legacy = serde_json::json!({
            "cookies": [],
            "origins": [{
                "origin": "https://app.example/",
                "localStorage": [["token", "jwt"]],
            }],
        });
        let (_, items) = ctx.set_storage_state(&legacy);
        assert_eq!(items, 1);
        // The trailing slash must normalize to the same key a page reads.
        assert_eq!(
            ctx.storage
                .get_item("https://app.example", StorageArea::Local, "token")
                .as_deref(),
            Some("jwt")
        );
    }

    #[test]
    fn storage_survives_a_context_restart_through_the_storage_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let first = BrowserContext::with_storage("a".to_string(), Some(path.clone()));
            first
                .storage
                .set_item("https://app.example", StorageArea::Local, "token", "jwt")
                .unwrap();
            first.save_cookies();
        }
        let second = BrowserContext::with_storage("b".to_string(), Some(path));
        assert_eq!(
            second
                .storage
                .get_item("https://app.example", StorageArea::Local, "token")
                .as_deref(),
            Some("jwt")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn isolated_copy_does_not_share_mutable_network_state() {
        let source = BrowserContext::with_full_options(
            "source".to_string(),
            None,
            false,
            Some("Template-UA/1.0".to_string()),
        );
        source.cookie_jar.set_cookie("sid=source", &url::Url::parse("https://example.com").unwrap());

        let persistent = source.isolated_copy("persistent".to_string(), true);
        let incognito = source.isolated_copy("incognito".to_string(), false);

        assert_eq!(persistent.cookie_jar.get_all_cookies().len(), 1);
        assert!(incognito.cookie_jar.get_all_cookies().is_empty());
        persistent.cookie_jar.clear();
        persistent.http_client.set_user_agent("Changed-UA/2.0").await;

        assert_eq!(source.cookie_jar.get_all_cookies().len(), 1);
        assert_eq!(source.http_client.user_agent.read().await.as_str(), "Template-UA/1.0");
    }
}
