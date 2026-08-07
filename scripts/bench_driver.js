// Drive one engine over a URL corpus for the benchmark harness.
//
// Deliberately identical for both engines: the same puppeteer-core client, the
// same waitUntil, the same viewport. Any difference in the numbers is then a
// difference between the engines rather than between two automation stacks.
//
// Usage: node bench_driver.js <wsEndpoint> <screenshot|extract> <url...>
const puppeteer = require('puppeteer-core');

const NAV_TIMEOUT_MS = 30000;

(async () => {
  const [ws, workload, ...urls] = process.argv.slice(2);
  const browser = await puppeteer.connect({ browserWSEndpoint: ws });
  const out = {};

  for (const url of urls) {
    const t0 = Date.now();
    let page;
    try {
      page = await browser.newPage();
      await page.setViewport({ width: 1280, height: 720 });
      await page.goto(url, { waitUntil: 'load', timeout: NAV_TIMEOUT_MS });

      let bytes = 0;
      if (workload === 'screenshot') {
        const buf = await page.screenshot();
        bytes = buf.length;
      } else {
        // "HTML extraction" = what an agent actually pulls off a page.
        const html = await page.content();
        bytes = html.length;
      }
      out[url] = { ok: true, ms: Date.now() - t0, bytes };
    } catch (e) {
      // A failure is recorded, never silently skipped: an engine that errors on
      // half the corpus would otherwise post excellent averages.
      out[url] = { ok: false, ms: Date.now() - t0, err: String(e.message || e).slice(0, 120) };
    } finally {
      if (page) { try { await page.close(); } catch { /* already gone */ } }
    }
  }

  await browser.disconnect();
  console.log(JSON.stringify(out));
  process.exit(0);
})().catch((e) => {
  console.log(JSON.stringify({ fatal: String(e).slice(0, 300) }));
  process.exit(1);
});
