//! Web Storage (`localStorage` / `sessionStorage`) areas, origin-keyed.
//!
//! Lives beside [`crate::cookies`] for one reason: it is the *other* half of a
//! durable session. Cookies alone do not carry a modern SaaS login — a large
//! share of SPAs keep their JWT in `localStorage`, so a browser that persists
//! cookies but not storage restores a session that looks logged in to the
//! server and logged out to the app.
//!
//! Design notes worth keeping:
//!
//! * **The origin is decided in Rust, from the page URL** — never passed up
//!   from JS. Page script must not be able to name the storage partition it
//!   reads, or one origin could harvest another's tokens.
//! * **Insertion order is preserved.** The spec leaves key order undefined, but
//!   `Object.keys(localStorage)` and `key(i)` observably follow insertion order
//!   in Chrome, and pages index into `key(i)`. A `HashMap` would make that
//!   order vary run to run and break replay determinism.
//! * **`sessionStorage` never reaches disk.** It is tab-scoped by definition;
//!   persisting it would resurrect state a real browser drops. It is still
//!   exportable, because an agent handing a session to another process wants it.
//! * **Writes are atomic** (temp file + rename, same as the cookie jar). Two
//!   pages in one context share this jar, and a torn `storage.json` silently
//!   poisons every future session that loads it.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// Per-origin, per-area byte budget. Chrome enforces ~5 MiB for `localStorage`;
/// matching it means a page that probes its quota (a common "am I in a real
/// browser?" check, and a real code path in offline-first apps) sees a
/// browser-shaped answer instead of unbounded success.
pub const QUOTA_BYTES: usize = 5 * 1024 * 1024;

/// Which Web Storage area a call addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StorageArea {
    Local,
    Session,
}

impl StorageArea {
    pub fn from_str_area(s: &str) -> Option<Self> {
        match s {
            "local" | "localStorage" => Some(StorageArea::Local),
            "session" | "sessionStorage" => Some(StorageArea::Session),
            _ => None,
        }
    }
}

/// An ordered key/value area for one origin.
type Area = Vec<(String, String)>;

#[derive(Default)]
struct Areas {
    local: Area,
    session: Area,
}

/// Thrown back to JS as `QuotaExceededError` when an item would push an area
/// past [`QUOTA_BYTES`].
#[derive(Debug)]
pub struct QuotaExceeded;

/// Origin-keyed Web Storage, shared by every page in a [`BrowserContext`].
///
/// [`BrowserContext`]: https://docs.rs/obscura-browser
#[derive(Default)]
pub struct StorageJar {
    origins: RwLock<BTreeMap<String, Areas>>,
}

/// The on-disk shape of `storage.json`. Only `localStorage` is written.
#[derive(Serialize, Deserialize)]
struct PersistedOrigin {
    origin: String,
    #[serde(default)]
    local_storage: Vec<PersistedItem>,
}

#[derive(Serialize, Deserialize)]
struct PersistedItem {
    name: String,
    value: String,
}

/// Normalize a page URL to a storage key (a serialized origin, e.g.
/// `https://example.com`). Returns `None` for opaque origins — `about:blank`,
/// `data:`, and anything unparseable — which get no persistent storage, exactly
/// as in a real browser.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    match parsed.scheme() {
        "http" | "https" | "ws" | "wss" | "ftp" => {}
        // file:// is opaque per spec but browsers in practice give each file
        // document a live area; keeping it memory-only avoids leaking local
        // page state into a shared profile.
        _ => return None,
    }
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{}:{}", parsed.scheme(), host, port),
        None => format!("{}://{}", parsed.scheme(), host),
    })
}

fn area_bytes(area: &Area) -> usize {
    area.iter().map(|(k, v)| k.len() + v.len()).sum()
}

impl StorageJar {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_area<R>(&self, origin: &str, area: StorageArea, f: impl FnOnce(&Area) -> R) -> R {
        let guard = self.origins.read().unwrap();
        match guard.get(origin) {
            Some(areas) => f(match area {
                StorageArea::Local => &areas.local,
                StorageArea::Session => &areas.session,
            }),
            None => f(&Vec::new()),
        }
    }

    fn with_area_mut<R>(
        &self,
        origin: &str,
        area: StorageArea,
        f: impl FnOnce(&mut Area) -> R,
    ) -> R {
        let mut guard = self.origins.write().unwrap();
        let areas = guard.entry(origin.to_string()).or_default();
        f(match area {
            StorageArea::Local => &mut areas.local,
            StorageArea::Session => &mut areas.session,
        })
    }

    /// All items for one origin/area, in insertion order.
    pub fn items(&self, origin: &str, area: StorageArea) -> Vec<(String, String)> {
        self.with_area(origin, area, |a| a.clone())
    }

    pub fn get_item(&self, origin: &str, area: StorageArea, key: &str) -> Option<String> {
        self.with_area(origin, area, |a| {
            a.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        })
    }

    /// Insert or update. Existing keys keep their position (Chrome behaviour);
    /// new keys append.
    pub fn set_item(
        &self,
        origin: &str,
        area: StorageArea,
        key: &str,
        value: &str,
    ) -> Result<(), QuotaExceeded> {
        self.with_area_mut(origin, area, |a| {
            let existing = a.iter().position(|(k, _)| k == key);
            let delta = match existing {
                Some(i) => {
                    (key.len() + value.len()) as isize - (a[i].0.len() + a[i].1.len()) as isize
                }
                None => (key.len() + value.len()) as isize,
            };
            if delta > 0 && area_bytes(a).saturating_add(delta as usize) > QUOTA_BYTES {
                return Err(QuotaExceeded);
            }
            match existing {
                Some(i) => a[i].1 = value.to_string(),
                None => a.push((key.to_string(), value.to_string())),
            }
            Ok(())
        })
    }

    pub fn remove_item(&self, origin: &str, area: StorageArea, key: &str) {
        self.with_area_mut(origin, area, |a| a.retain(|(k, _)| k != key));
    }

    pub fn clear(&self, origin: &str, area: StorageArea) {
        self.with_area_mut(origin, area, |a| a.clear());
    }

    /// Replace an origin/area wholesale (import path).
    pub fn replace(&self, origin: &str, area: StorageArea, items: Vec<(String, String)>) {
        self.with_area_mut(origin, area, |a| *a = items);
    }

    /// Every origin that holds at least one item in either area.
    pub fn origins(&self) -> Vec<String> {
        self.origins
            .read()
            .unwrap()
            .iter()
            .filter(|(_, areas)| !areas.local.is_empty() || !areas.session.is_empty())
            .map(|(origin, _)| origin.clone())
            .collect()
    }

    /// Snapshot every origin's `localStorage`. Used to compute the write-back
    /// delta when a connection-scoped copy merges into a shared persistence
    /// template: last-writer-wins on the whole jar would let an untouched
    /// origin in one connection erase another connection's fresh login.
    pub fn snapshot_local(&self) -> BTreeMap<String, Vec<(String, String)>> {
        self.origins
            .read()
            .unwrap()
            .iter()
            .map(|(origin, areas)| (origin.clone(), areas.local.clone()))
            .collect()
    }

    /// Drop everything for one origin (both areas) — CDP
    /// `Storage.clearDataForOrigin`.
    pub fn clear_origin(&self, origin: &str) {
        self.origins.write().unwrap().remove(origin);
    }

    pub fn clear_all(&self) {
        self.origins.write().unwrap().clear();
    }

    /// Copy every area from `other` into self (used when forking a context).
    pub fn copy_from(&self, other: &StorageJar) {
        let src = other.origins.read().unwrap();
        let mut dst = self.origins.write().unwrap();
        for (origin, areas) in src.iter() {
            dst.insert(
                origin.clone(),
                Areas {
                    local: areas.local.clone(),
                    session: areas.session.clone(),
                },
            );
        }
    }

    /// Persist `localStorage` only, atomically. `sessionStorage` is
    /// deliberately excluded — see the module docs.
    pub fn save_to_file(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let guard = self.origins.read().unwrap();
        let out: Vec<PersistedOrigin> = guard
            .iter()
            .filter(|(_, areas)| !areas.local.is_empty())
            .map(|(origin, areas)| PersistedOrigin {
                origin: origin.clone(),
                local_storage: areas
                    .local
                    .iter()
                    .map(|(name, value)| PersistedItem {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            })
            .collect();
        drop(guard);

        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp =
            tempfile::NamedTempFile::new_in(path.parent().unwrap_or(std::path::Path::new(".")))?;
        tmp.write_all(json.as_bytes())?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Load `localStorage` from disk, merging into whatever is already held.
    /// Returns the number of items loaded.
    pub fn load_from_file(&self, path: &std::path::Path) -> Result<usize, std::io::Error> {
        if !path.exists() {
            return Ok(0);
        }
        let data = std::fs::read_to_string(path)?;
        let parsed: Vec<PersistedOrigin> = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut count = 0usize;
        for entry in parsed {
            for item in entry.local_storage {
                if self
                    .set_item(&entry.origin, StorageArea::Local, &item.name, &item.value)
                    .is_ok()
                {
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}

/// File name used inside a context's `storage_dir`, beside `cookies.json`.
pub const STORAGE_FILE: &str = "storage.json";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_normalizes_and_rejects_opaque() {
        assert_eq!(
            origin_of("https://example.com/a/b?c=1").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            origin_of("http://localhost:8080/x").as_deref(),
            Some("http://localhost:8080")
        );
        // Default ports stay implicit so https://x and https://x:443 agree.
        assert_eq!(
            origin_of("https://example.com:443/").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(origin_of("about:blank"), None);
        assert_eq!(origin_of("data:text/html,hi"), None);
        assert_eq!(origin_of("file:///tmp/x.html"), None);
        assert_eq!(origin_of("not a url"), None);
    }

    #[test]
    fn origins_are_isolated() {
        let jar = StorageJar::new();
        jar.set_item("https://a.com", StorageArea::Local, "tok", "A")
            .unwrap();
        jar.set_item("https://b.com", StorageArea::Local, "tok", "B")
            .unwrap();
        assert_eq!(
            jar.get_item("https://a.com", StorageArea::Local, "tok")
                .as_deref(),
            Some("A")
        );
        assert_eq!(
            jar.get_item("https://b.com", StorageArea::Local, "tok")
                .as_deref(),
            Some("B")
        );
        // Same-origin areas are separate too.
        assert_eq!(
            jar.get_item("https://a.com", StorageArea::Session, "tok"),
            None
        );
    }

    #[test]
    fn insertion_order_is_stable_and_update_keeps_position() {
        let jar = StorageJar::new();
        for k in ["z", "a", "m"] {
            jar.set_item("https://x.com", StorageArea::Local, k, "1")
                .unwrap();
        }
        jar.set_item("https://x.com", StorageArea::Local, "z", "2")
            .unwrap();
        let items = jar.items("https://x.com", StorageArea::Local);
        assert_eq!(
            items,
            vec![
                ("z".into(), "2".into()),
                ("a".into(), "1".into()),
                ("m".into(), "1".into())
            ]
        );
    }

    #[test]
    fn quota_is_enforced_and_shrinking_writes_still_pass() {
        let jar = StorageJar::new();
        let big = "x".repeat(QUOTA_BYTES - 10);
        jar.set_item("https://x.com", StorageArea::Local, "k", &big)
            .unwrap();
        assert!(jar
            .set_item("https://x.com", StorageArea::Local, "k2", &"y".repeat(64))
            .is_err());
        // Overwriting with something smaller must not be refused.
        jar.set_item("https://x.com", StorageArea::Local, "k", "small")
            .unwrap();
        jar.set_item("https://x.com", StorageArea::Local, "k2", "ok")
            .unwrap();
    }

    #[test]
    fn roundtrip_persists_local_but_not_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage.json");
        let jar = StorageJar::new();
        jar.set_item("https://x.com", StorageArea::Local, "tok", "abc")
            .unwrap();
        jar.set_item("https://x.com", StorageArea::Session, "tmp", "nope")
            .unwrap();
        jar.save_to_file(&path).unwrap();

        let restored = StorageJar::new();
        assert_eq!(restored.load_from_file(&path).unwrap(), 1);
        assert_eq!(
            restored
                .get_item("https://x.com", StorageArea::Local, "tok")
                .as_deref(),
            Some("abc")
        );
        assert_eq!(
            restored.get_item("https://x.com", StorageArea::Session, "tmp"),
            None
        );
    }

    #[test]
    fn load_from_missing_file_is_not_an_error() {
        let jar = StorageJar::new();
        assert_eq!(
            jar.load_from_file(std::path::Path::new("/nonexistent/storage.json"))
                .unwrap(),
            0
        );
    }

    #[test]
    fn clear_origin_drops_both_areas() {
        let jar = StorageJar::new();
        jar.set_item("https://x.com", StorageArea::Local, "a", "1")
            .unwrap();
        jar.set_item("https://x.com", StorageArea::Session, "b", "2")
            .unwrap();
        jar.clear_origin("https://x.com");
        assert!(jar.items("https://x.com", StorageArea::Local).is_empty());
        assert!(jar.items("https://x.com", StorageArea::Session).is_empty());
        assert!(jar.origins().is_empty());
    }
}
