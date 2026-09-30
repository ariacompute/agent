#!/usr/bin/env node
/*
 * Universal browser runner for the aria agent browser sandbox.
 *
 * Invoked by `BrowserSandbox` as:
 *     node /runner/runner.mjs '<BrowserRequest JSON>'
 *
 * where BrowserRequest = { "engine": "chromium"|"camoufox"|"lightpanda"|
 *                          "obscura"|"servo"|"gosub",
 *                          "op": { "op": "navigate", "url": "..." }, ... }
 *
 * It writes a single JSON line to stdout:
 *     { "ok": true, "data": { ... } }            on success
 *     { "ok": false, "error": "..." }            on failure
 *
 * Native engines (servo, gosub) only support navigate + extract; every other
 * op returns { "ok": false, "error": "unsupported ..." } so the Rust side maps
 * it to BrowserError::Unsupported instead of panicking.
 */

const { chromium, firefox } = (() => {
  try {
    return require("playwright");
  } catch {
    return {};
  }
})();

const child_process = require("child_process");

function send(result) {
  process.stdout.write(JSON.stringify(result) + "\n");
}

async function runPlaywright(engine, op) {
  // Select a browser type and launch options per engine.
  let browserType = chromium;
  const launchOpts = { args: [] };
  if (engine === "camoufox") {
    try {
      // camoufox ships a playwright-compatible launcher.
      const camoufox = require("camoufox");
      if (camoufox && camoufox.launch) {
        return runWithLauncher(() => camoufox.launch(launchOpts), op);
      }
    } catch {
      // Fall back to Firefox if camoufox isn't installed.
    }
    browserType = firefox;
  } else if (engine === "obscura") {
    // Stealth / anti-bot fingerprinting mitigations.
    launchOpts.args.push(
      "--disable-blink-features=AutomationControlled",
      "--no-sandbox",
      "--disable-dev-shm-usage"
    );
  } else if (engine === "lightpanda") {
    // Lightpanda exposes a Playwright-compatible protocol; drive via chromium.
    // (Set BROWSER_BIN to the lightpanda binary to use it directly.)
    if (process.env.BROWSER_BIN) {
      launchOpts.executablePath = process.env.BROWSER_BIN;
    }
  }
  return runWithLauncher(() => browserType.launch(launchOpts), op);
}

async function runWithLauncher(launchFn, op) {
  let browser;
  try {
    browser = await launchFn();
  } catch (e) {
    return { ok: false, error: `launch failed: ${e.message}` };
  }
  try {
    const page = await browser.newPage();
    return await dispatch(page, op);
  } catch (e) {
    return { ok: false, error: e.message };
  } finally {
    await browser.close().catch(() => {});
  }
}

async function dispatch(page, op) {
  switch (op.op) {
    case "navigate": {
      if (!op.url) return { ok: false, error: "unsupported: navigate requires url" };
      await page.goto(op.url, { waitUntil: "load", timeout: 30000 });
      return { ok: true, data: { title: await page.title() } };
    }
    case "extract": {
      const text = await page.evaluate(() => document.body?.innerText || "");
      const html = await page.content();
      return { ok: true, data: { title: await page.title(), text, html } };
    }
    case "click": {
      if (!op.selector) return { ok: false, error: "unsupported: click requires selector" };
      await page.click(op.selector, { timeout: 10000 });
      return { ok: true, data: { title: await page.title() } };
    }
    case "fill": {
      if (!op.selector || op.text === undefined)
        return { ok: false, error: "unsupported: fill requires selector + text" };
      await page.fill(op.selector, op.text);
      return { ok: true, data: {} };
    }
    case "screenshot": {
      const buf = await page.screenshot({ type: "png" });
      return { ok: true, data: { screenshot: buf.toString("base64") } };
    }
    case "evaluate": {
      if (!op.js) return { ok: false, error: "unsupported: evaluate requires js" };
      const value = await page.evaluate(op.js);
      return { ok: true, data: { value } };
    }
    case "solve_captcha": {
      // Hook for the playwright-captcha integration. Returns a no-op result when
      // the integration is not wired up so the agent can still proceed.
      try {
        const cap = require("playwright-captcha");
        if (cap && typeof cap.solve === "function") {
          const res = await cap.solve(page);
          return { ok: true, data: { solved: true, detail: res } };
        }
      } catch {
        /* integration absent */
      }
      return { ok: true, data: { solved: false, detail: "no captcha solver configured" } };
    }
    default:
      return { ok: false, error: `unsupported op: ${op.op}` };
  }
}

async function runNative(engine, op) {
  // Servo / Gosub are driven through their headless CLI for navigate + extract.
  const bin = engine === "gosub" ? "gosub" : "servo";
  if (op.op === "navigate") {
    if (!op.url) return { ok: false, error: "unsupported: navigate requires url" };
    child_process.execSync(`${bin} ${JSON.stringify(op.url)}`, { stdio: "ignore" });
    return { ok: true, data: { title: op.url } };
  }
  if (op.op === "extract") {
    const out = child_process
      .execSync(`${bin} --dump`, { encoding: "utf8" })
      .catch(() => "");
    return { ok: true, data: { text: out } };
  }
  return { ok: false, error: `unsupported op ${op.op} for ${engine}` };
}

(async () => {
  try {
    const raw = process.argv[2];
    if (!raw) {
      send({ ok: false, error: "missing request argument" });
      return;
    }
    const req = JSON.parse(raw);
    const { engine, op } = req;
    if (engine === "servo" || engine === "gosub") {
      send(await runNative(engine, op || {}));
      return;
    }
    if (!chromium && !firefox) {
      send({ ok: false, error: "playwright is not installed in this image" });
      return;
    }
    send(await runPlaywright(engine, op || {}));
  } catch (e) {
    send({ ok: false, error: `runner error: ${e.message}` });
  }
})();
