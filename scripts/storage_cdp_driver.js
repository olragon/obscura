// Puppeteer driver for the storage checks. Run via scripts/storage_live_check.py.
//
// Real Puppeteer on purpose: a hand-rolled WebSocket client talks to the
// dispatcher, which is not the same thing as talking to the client library
// everyone actually uses — the fast-path bug found in BROWSER-129 was invisible
// to dispatch-level tests and only a real client caught it.
//
// Prints one JSON line: {checks: [[name, ok, detail], ...]}
const puppeteer = require("puppeteer-core");

const [, , cdpBase, siteBase] = process.argv;
const checks = [];
const check = (name, ok, detail = "") => checks.push([name, !!ok, String(detail).slice(0, 200)]);

(async () => {
  let browser;
  try {
    browser = await puppeteer.connect({ browserURL: cdpBase, defaultViewport: null });
    const page = await browser.newPage();

    // Order matters: `page.createCDPSession()` before the first navigation
    // wedges `Page.navigate` in this engine (reproduced on a build without any
    // of this work, so it is pre-existing and unrelated to storage). Navigate
    // first, then attach the raw session.
    await page.goto(`${siteBase}/read.html`);
    const client = await page.createCDPSession();

    // 1. Seed via DOMStorage, then prove a fresh document picks it up — the
    //    "restore a login before the app boots" path.
    const storageId = { securityOrigin: siteBase, isLocalStorage: true };
    await client.send("DOMStorage.enable");
    await client.send("DOMStorage.setDOMStorageItem", {
      storageId,
      key: "seeded",
      value: "from-cdp",
    });
    await page.reload();
    const seen = await page.evaluate(() => localStorage.getItem("seeded"));
    check("DOMStorage.setDOMStorageItem is visible to page script", seen === "from-cdp", seen);

    // 2. Page writes; DOMStorage.getDOMStorageItems reads it back.
    await page.evaluate(() => localStorage.setItem("from-page", "roundtrip"));
    const { entries } = await client.send("DOMStorage.getDOMStorageItems", { storageId });
    const map = Object.fromEntries(entries);
    check("getDOMStorageItems sees a page-side write", map["from-page"] === "roundtrip", JSON.stringify(entries));
    check("getDOMStorageItems still has the seeded key", map.seeded === "from-cdp", JSON.stringify(entries));

    // 3. Removal and clear.
    await client.send("DOMStorage.removeDOMStorageItem", { storageId, key: "seeded" });
    const afterRemove = await page.evaluate(() => localStorage.getItem("seeded"));
    check("removeDOMStorageItem removes it for the page too", afterRemove === null, afterRemove);

    // 4. Origin isolation over the wire: a different origin's area is separate.
    const otherId = { securityOrigin: "https://other.example", isLocalStorage: true };
    const other = await client.send("DOMStorage.getDOMStorageItems", { storageId: otherId });
    check("another origin's area is empty", other.entries.length === 0, JSON.stringify(other.entries));

    // 5. sessionStorage is a distinct area under the same origin.
    const sessionId = { securityOrigin: siteBase, isLocalStorage: false };
    await client.send("DOMStorage.setDOMStorageItem", { storageId: sessionId, key: "s", value: "1" });
    const localAfter = await client.send("DOMStorage.getDOMStorageItems", { storageId });
    check(
      "sessionStorage write does not land in localStorage",
      !Object.fromEntries(localAfter.entries).s,
      JSON.stringify(localAfter.entries)
    );
    const sessionSeen = await page.evaluate(() => sessionStorage.getItem("s"));
    check("sessionStorage write is visible to page script", sessionSeen === "1", sessionSeen);

    // 6. Storage.clearDataForOrigin wipes the origin.
    await client.send("Storage.clearDataForOrigin", { origin: siteBase, storageTypes: "all" });
    const cleared = await client.send("DOMStorage.getDOMStorageItems", { storageId });
    check("clearDataForOrigin empties the origin", cleared.entries.length === 0, JSON.stringify(cleared.entries));

    // 7. Quota is enforced with the right error name.
    const quota = await page.evaluate(() => {
      try {
        localStorage.setItem("big", "x".repeat(6 * 1024 * 1024));
        return "no-throw";
      } catch (e) {
        return e.name;
      }
    });
    check("over-quota write throws QuotaExceededError", quota === "QuotaExceededError", quota);

    // 8. The WHATWG surface still behaves (length/key/enumeration) on the
    //    op-backed store — the shim was rewritten, so re-prove it.
    const surface = await page.evaluate(() => {
      localStorage.clear();
      localStorage.setItem("a", "1");
      localStorage.b = "2";
      return {
        length: localStorage.length,
        key0: localStorage.key(0),
        dotRead: localStorage.b,
        keys: Object.keys(localStorage),
        hasA: "a" in localStorage,
        afterDelete: (() => {
          delete localStorage.a;
          return localStorage.getItem("a");
        })(),
      };
    });
    check("length/key/dot-access/enumeration intact",
      surface.length === 2 &&
      surface.key0 === "a" &&
      surface.dotRead === "2" &&
      surface.keys.join(",") === "a,b" &&
      surface.hasA === true &&
      surface.afterDelete === null,
      JSON.stringify(surface));

    await browser.disconnect();
  } catch (err) {
    check("driver completed", false, err && err.message);
    if (browser) { try { await browser.disconnect(); } catch (_) {} }
  }
  console.log(JSON.stringify({ checks }));
  // A live CDP connection keeps the event loop alive; exit explicitly.
  process.exit(0);
})();
