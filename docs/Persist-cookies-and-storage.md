`--storage-dir` persists cookies and `localStorage` to disk so they survive across runs.

## CLI

```bash
obscura --allow-private-network --storage-dir ./obscura-data fetch https://example.com
obscura --storage-dir ./obscura-data fetch https://example.com
```

The second invocation starts with the cookies and `localStorage` left by the first.
`--storage-dir` is a **global** flag: it goes before the subcommand, and applies to
`fetch`, `serve`, and `mcp` alike.

## Server

```bash
obscura --storage-dir ./obscura-data serve
```

All CDP sessions read and write to the same directory. Each connection gets an isolated copy of the context and writes back only *its own* changes (per cookie, and per origin/key for storage), so two concurrent connections cannot silently clobber each other's session. Run separate `obscura serve` processes with different `--storage-dir` paths for isolated profiles.

## MCP

```bash
obscura --storage-dir ./obscura-data mcp
```

Without a storage directory an MCP session is ephemeral — a login dies with the process.

## Layout

Inside `./obscura-data`:

- `cookies.json`: cookie jar in a stable format with `same_site`, `expires`, `http_only`, `secure`.
- `storage.json`: `localStorage`, one entry per origin: `[{origin, local_storage: [{name, value}]}]`.

Both files are written atomically (temp file + rename), so a killed process cannot leave a
half-written jar that poisons every later session.

Inspect with `jq`:

```bash
jq '.[] | select(.domain == "example.com")' ./obscura-data/cookies.json
jq '.[] | select(.origin == "https://example.com")' ./obscura-data/storage.json
```

## What is and is not persisted

| State | Persisted | Notes |
|---|---|---|
| Cookies | yes | including `HttpOnly` |
| `localStorage` | yes | per origin; 5 MiB per origin, as in Chrome |
| `sessionStorage` | no | tab-scoped by definition; live for the process, exportable via `storageState`, never written to disk |
| IndexedDB | no | not implemented — Playwright does not capture it either |
| Service Worker / Cache Storage | no | same |

## When state is written

- After every MCP tool call.
- When a CDP connection closes (its delta is merged into the shared profile).
- On clean process exit of `serve` / `fetch`.

## `storageState` — portable sessions

The MCP tools `browser_storage_state` / `browser_set_storage_state` export and import a
**Playwright-shaped** object:

```json
{
  "cookies": [{"name": "sid", "value": "…", "domain": "app.example", "path": "/",
               "expires": -1, "httpOnly": true, "secure": true, "sameSite": "Lax"}],
  "origins": [{"origin": "https://app.example",
               "localStorage": [{"name": "token", "value": "jwt…"}]}]
}
```

Because the shape matches, a session captured with Playwright's `context.storageState()` imports
here unchanged, and vice versa. Import is Rust-native: it applies **before** any navigation, and
covers every origin in the file — not only the one currently loaded.

## Reading and writing storage over CDP

The `DOMStorage` domain is implemented, so a CDP client can seed or read storage without injecting
script:

```js
const client = await page.createCDPSession();          // after the first navigation
const storageId = { securityOrigin: "https://app.example", isLocalStorage: true };
await client.send("DOMStorage.setDOMStorageItem", { storageId, key: "token", value: "jwt…" });
const { entries } = await client.send("DOMStorage.getDOMStorageItems", { storageId });
```

`Storage.clearDataForOrigin` clears cookies and web storage for an origin (`storageTypes` of
`all`, `cookies`, or `local_storage`).

## Login once, scrape many

```bash
obscura --storage-dir ./session-1 serve
```

Drive a login flow once via Puppeteer. Stop the server. Subsequent runs against the same
`--storage-dir` start logged in. Validate before trusting: replay the saved state against an
authenticated route and re-login on failure — tokens rotate and expire, and a stale session fails
in ways that look like a broken scraper.

## Multiple identities

```bash
obscura --port 9222 --storage-dir ./identity-a serve
obscura --port 9223 --storage-dir ./identity-b serve
```

## Clear state

```bash
rm -rf ./obscura-data
```
