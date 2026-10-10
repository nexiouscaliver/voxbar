#!/usr/bin/env node
/*
 * VoxBar UI screenshot rig.
 *
 * Re-runnable driver that serves the built frontend (dist/) on a local
 * port, loads it in headless Chromium with the browser-side Tauri IPC mock
 * (ui-shots/tauri-mock-init.js) injected before any app code, walks every
 * onboarding step, every settings tab, the footer, and the statically
 * reachable overlay/toast states, and writes PNG screenshots plus a per
 * screen DOM audit (font sizes, text/background contrast, overflow and
 * clipping, spacing consistency) as JSON next to the shots.
 *
 * Usage:
 *   node ui-shots/shoot.mjs [outputDir] [--port 4317] [--only filter]
 *
 * Requires `bun run build` to have produced dist/ (the script refuses to
 * run against a stale or missing build only if dist/index.html is absent).
 */

import { createServer } from "node:http";
import { readFile, stat, mkdir, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const here = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.resolve(here, "..");
const distDir = path.join(projectRoot, "dist");
const mockPath = path.join(here, "tauri-mock-init.js");

// --- CLI ---
const argv = process.argv.slice(2);
const flags = {};
for (let i = 0; i < argv.length; i += 1) {
  if (argv[i].startsWith("--")) {
    flags[argv[i].slice(2)] =
      argv[i + 1] && !argv[i + 1].startsWith("--") ? argv[i + 1] : true;
    if (flags[argv[i].slice(2)] !== true) i += 1;
  }
}
const outDir = path.resolve(
  projectRoot,
  argv.find((a) => !a.startsWith("--")) || "ui-shots/round0",
);
const port = Number(flags.port || 4317);
const only = typeof flags.only === "string" ? flags.only : null;

if (!existsSync(path.join(distDir, "index.html"))) {
  console.error(
    `dist/index.html not found under ${distDir}. Run: bun run build`,
  );
  process.exit(1);
}

// --- static server for dist/ ---
const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".woff2": "font/woff2",
  ".md": "text/markdown; charset=utf-8",
};
const server = createServer(async (req, res) => {
  try {
    const urlPath = decodeURIComponent(
      new URL(req.url, "http://localhost").pathname,
    );
    let filePath = path.join(distDir, urlPath);
    if (!existsSync(filePath)) {
      res.writeHead(404).end("not found");
      return;
    }
    const st = await stat(filePath);
    if (st.isDirectory()) filePath = path.join(filePath, "index.html");
    const body = await readFile(filePath);
    res.writeHead(200, {
      "content-type":
        MIME[path.extname(filePath)] || "application/octet-stream",
    });
    res.end(body);
  } catch {
    res.writeHead(500).end("server error");
  }
});
await new Promise((resolve) => server.listen(port, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${port}`;
console.log(`serving ${distDir} at ${base}`);

// --- DOM audit (runs inside the page after each shot) ---
const auditScript = () => {
  const parseColor = (value) => {
    const m = value?.match(
      /rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)(?:[\s,/]+([\d.]+))?\s*\)/,
    );
    if (!m) return null;
    return {
      r: Number(m[1]),
      g: Number(m[2]),
      b: Number(m[3]),
      a: m[4] === undefined ? 1 : Number(m[4]),
    };
  };
  const lum = (c) => {
    const f = (v) => {
      const s = v / 255;
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b);
  };
  const ratio = (a, b) => {
    const l1 = lum(a);
    const l2 = lum(b);
    return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
  };
  const blend = (fg, bg) => ({
    r: fg.r * fg.a + bg.r * (1 - fg.a),
    g: fg.g * fg.a + bg.g * (1 - fg.a),
    b: fg.b * fg.a + bg.b * (1 - fg.a),
    a: 1,
  });
  const selectorOf = (el) => {
    const parts = [];
    let node = el;
    while (node && node.nodeType === 1 && parts.length < 5) {
      let part = node.tagName.toLowerCase();
      if (node.id) part += `#${node.id}`;
      const cls =
        node.className && typeof node.className === "string"
          ? node.className.trim().split(/\s+/).slice(0, 3).join(".")
          : "";
      if (cls) part += `.${cls}`;
      parts.unshift(part);
      node = node.parentElement;
    }
    return parts.join(" > ");
  };
  const isVisible = (el) => {
    const style = getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 2 && rect.height > 2;
  };
  // True when the element lives inside a scrollable box (the document or an
  // overflow container): below-the-fold content there is expected layout,
  // not a defect, so the viewport check must not flag it.
  const insideScrollable = (el, axis) => {
    const doc = document.documentElement;
    const prop = axis === "y" ? "scrollHeight" : "scrollWidth";
    const clientProp = axis === "y" ? "clientHeight" : "clientWidth";
    if (doc[prop] > doc[clientProp] + 2) return true;
    let node = el.parentElement;
    while (node && node.nodeType === 1) {
      const style = getComputedStyle(node);
      const overflow = axis === "y" ? style.overflowY : style.overflowX;
      if (
        (overflow === "auto" || overflow === "scroll") &&
        node[prop] > node[clientProp] + 2
      ) {
        return true;
      }
      node = node.parentElement;
    }
    return false;
  };
  const effectiveBackground = (el) => {
    let acc = { r: 255, g: 255, b: 255, a: 0 };
    let node = el;
    let gradientSource = null;
    while (node && node.nodeType === 1) {
      const style = getComputedStyle(node);
      if (style.backgroundImage && style.backgroundImage !== "none") {
        gradientSource = selectorOf(node);
      }
      const color = parseColor(style.backgroundColor);
      if (color && color.a > 0) {
        acc =
          color.a >= 1
            ? color
            : blend(color, acc.a > 0 ? acc : { r: 255, g: 255, b: 255, a: 1 });
        if (acc.a >= 1) break;
      }
      node = node.parentElement;
    }
    return {
      color: acc.a > 0 ? acc : { r: 255, g: 255, b: 255, a: 1 },
      gradientSource,
    };
  };

  const fontBuckets = new Map();
  const contrastFindings = [];
  const contrastMeasured = { pairs: 0 };
  const overflowFindings = [];
  const spacingCounts = new Map();
  const all = Array.from(document.querySelectorAll("body *")).slice(0, 4000);

  for (const el of all) {
    if (!isVisible(el)) continue;
    const style = getComputedStyle(el);

    // Font sizes of elements that directly render text.
    const ownText = Array.from(el.childNodes).some(
      (n) => n.nodeType === 3 && n.textContent.trim().length > 0,
    );
    if (ownText) {
      const size = style.fontSize;
      const entry = fontBuckets.get(size) || {
        size,
        count: 0,
        examples: [],
        weights: new Set(),
      };
      entry.count += 1;
      entry.weights.add(style.fontWeight);
      if (entry.examples.length < 3) {
        entry.examples.push(
          `${selectorOf(el)} :: ${el.textContent.trim().slice(0, 40)}`,
        );
      }
      fontBuckets.set(size, entry);

      // Contrast of that text against its effective background.
      const fg = parseColor(style.color);
      const bg = effectiveBackground(el);
      if (fg) {
        contrastMeasured.pairs += 1;
        const px = parseFloat(size);
        const large =
          px >= 24 || (px >= 18.66 && Number(style.fontWeight) >= 700);
        const fgResolved = fg.a < 1 ? blend(fg, bg.color) : fg;
        const r = ratio(fgResolved, bg.color);
        const threshold = large ? 3.0 : 4.5;
        if (r < threshold) {
          contrastFindings.push({
            selector: selectorOf(el),
            text: el.textContent.trim().slice(0, 60),
            fontSize: size,
            fontWeight: style.fontWeight,
            color: style.color,
            backgroundColor: `rgb(${Math.round(bg.color.r)} ${Math.round(
              bg.color.g,
            )} ${Math.round(bg.color.b)})`,
            backgroundFromGradient: Boolean(bg.gradientSource),
            ratio: Math.round(r * 100) / 100,
            threshold,
            largeText: large,
          });
        }
      }
    }

    // Overflow and clipping.
    const sw = el.scrollWidth - el.clientWidth;
    const sh = el.scrollHeight - el.clientHeight;
    const scrollsX = ["auto", "scroll"].includes(style.overflowX);
    const scrollsY = ["auto", "scroll"].includes(style.overflowY);
    if ((sw > 1 && !scrollsX) || (sh > 1 && !scrollsY)) {
      overflowFindings.push({
        selector: selectorOf(el),
        kind:
          style.overflowX === "hidden" || style.overflowY === "hidden"
            ? "clipped"
            : "overflowing",
        scrollWidth: el.scrollWidth,
        clientWidth: el.clientWidth,
        scrollHeight: el.scrollHeight,
        clientHeight: el.clientHeight,
        text: el.textContent.trim().slice(0, 50),
      });
    }
    const doc = document.documentElement;
    const rect = el.getBoundingClientRect();
    // Below-fold or past-the-edge content inside a scrollable box (the
    // document itself or an overflow container) is expected layout, not a
    // defect: only flag what the user could never scroll to.
    const scrollableY = insideScrollable(el, "y");
    const scrollableX = insideScrollable(el, "x");
    if (
      (!scrollableY && rect.top < -2) ||
      (!scrollableX && rect.left < -2) ||
      (!scrollableX && rect.right > doc.clientWidth + 2) ||
      (!scrollableY && rect.bottom > doc.clientHeight + 2)
    ) {
      overflowFindings.push({
        selector: selectorOf(el),
        kind: "out-of-viewport",
        rect: {
          left: Math.round(rect.left),
          top: Math.round(rect.top),
          right: Math.round(rect.right),
          bottom: Math.round(rect.bottom),
        },
        document: {
          w: doc.scrollWidth,
          h: doc.scrollHeight,
        },
        viewport: { w: doc.clientWidth, h: doc.clientHeight },
        text: el.textContent.trim().slice(0, 50),
      });
    }

    // Spacing inventory (margins, paddings, gap) for consistency review.
    for (const prop of [
      "marginTop",
      "marginBottom",
      "paddingTop",
      "paddingBottom",
      "paddingLeft",
      "paddingRight",
      "columnGap",
      "rowGap",
    ]) {
      const v = style[prop];
      if (!v || v === "0px" || v === "normal" || v === "auto") continue;
      const key = `${prop}=${v}`;
      const entry = spacingCounts.get(key) || {
        property: prop,
        value: v,
        count: 0,
      };
      entry.count += 1;
      spacingCounts.set(key, entry);
    }
  }

  const spacingList = Array.from(spacingCounts.values()).sort(
    (a, b) => b.count - a.count,
  );
  const pxValue = (v) => Number(String(v).replace(/[^\d.]/g, "")) || 0;
  return {
    fontSizes: Array.from(fontBuckets.values())
      .sort((a, b) => b.count - a.count)
      .map((e) => ({
        size: e.size,
        count: e.count,
        weights: Array.from(e.weights),
        examples: e.examples,
      })),
    contrast: {
      measuredPairs: contrastMeasured.pairs,
      failing: contrastFindings.sort((a, b) => a.ratio - b.ratio).slice(0, 40),
      failingCount: contrastFindings.length,
    },
    overflow: {
      findings: overflowFindings.slice(0, 40),
      count: overflowFindings.length,
    },
    spacing: {
      distinctValues: spacingList.slice(0, 60),
      // Odd non-multiples of 4 used more than zero times: candidates for
      // inconsistent spacing (tailwind's scale is 4px-based).
      offScale: spacingList.filter(
        (s) => pxValue(s.value) > 0 && pxValue(s.value) % 4 !== 0,
      ),
    },
  };
};

// --- shot definitions ---
const MAIN = "index.html";
const OVERLAY = "src/overlay/index.html";
const settle = (ms) => new Promise((r) => setTimeout(r, ms));

/** Wait for the settings shell; the driver has already navigated. */
async function openMain(page) {
  await page.waitForSelector("#root div", { timeout: 15000 });
  await page.waitForFunction(
    () => document.querySelectorAll("body *").length > 20,
    { timeout: 15000 },
  );
  await page.evaluate(() => document.fonts.ready);
  await settle(900);
}

async function clickSidebar(page, title) {
  await page.click(`p[title="${title}"]`, { timeout: 5000 });
  await settle(700);
}

const shots = [
  // ---- onboarding ----
  {
    name: "onboarding-accessibility-fresh",
    scenario: "onboarding-fresh",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await settle(1200);
    },
  },
  {
    name: "onboarding-accessibility-mic-pending",
    scenario: "onboarding-mic",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await settle(1200);
    },
  },
  {
    name: "onboarding-model-step",
    scenario: "onboarding-model",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      // Permissions are granted, so the wizard auto-advances (~0.3s timer).
      await page.waitForSelector("#root div", { timeout: 15000 });
      await page.waitForFunction(() => document.body.innerText.length > 200, {
        timeout: 15000,
      });
      await settle(1600);
    },
  },

  // ---- settings tabs (default user: debug off, post-process off) ----
  ...[
    ["general", "General"],
    ["commands", "Commands"],
    ["history", "History"],
    ["models", "Models"],
    ["output", "Output"],
    ["advanced", "Advanced"],
    ["about", "About"],
  ].map(([tab, title]) => ({
    name: `settings-${tab}`,
    scenario: "settings-default",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await openMain(page);
      await clickSidebar(page, title);
    },
  })),

  // ---- gated tabs (debug + post-process enabled) ----
  ...[
    ["postprocessing", "Post Process"],
    ["debug", "Debug"],
  ].map(([tab, title]) => ({
    name: `settings-${tab}`,
    scenario: "settings-full",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await openMain(page);
      await clickSidebar(page, title);
    },
  })),

  // ---- footer (clipped to the footer element on the general tab) ----
  {
    name: "footer",
    scenario: "settings-default",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await openMain(page);
    },
    clipTo: async (page) => {
      const el = page.locator("#root > div > div:last-child").first();
      await el.waitFor({ timeout: 5000 });
      return el;
    },
  },

  // ---- settings search dropdown ----
  {
    name: "settings-search-dropdown",
    scenario: "settings-default",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await openMain(page);
      const input = page.locator('input[placeholder*="Search"]').first();
      await input.click({ timeout: 5000 });
      await input.fill("microphone");
      await settle(600);
    },
  },

  // ---- What's New modal ----
  {
    name: "whats-new-modal",
    scenario: "whats-new",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await openMain(page);
      await page
        .waitForFunction(() => document.body.innerText.includes("What"), {
          timeout: 8000,
        })
        .catch(() => {});
      await settle(1200);
    },
  },

  // ---- toast states (main window; each on a fresh page for dedupe) ----
  ...[
    [
      "toast-error-recording",
      [
        [
          "recording-error",
          { error_type: "microphone_permission_denied", detail: null },
        ],
      ],
    ],
    [
      "toast-warning-model-fallback",
      [
        [
          "model-fallback",
          {
            fallback_model_name: "Canary 180M Flash",
            refused_model_name: "Voxtral Mini 4B",
          },
        ],
      ],
    ],
    [
      "toast-info-post-process-skip",
      [
        [
          "post-process-skip-event",
          { reason: "memory_gate", detail: "2.4 GB free, model needs 3.1 GB" },
        ],
      ],
    ],
    [
      "toast-stack-mixed",
      [
        ["recording-error", { error_type: "no_model_selected", detail: null }],
        ["command-mode-no-session", null],
        ["post-process-skip-event", { reason: "timeout" }],
      ],
    ],
  ].map(([name, events]) => ({
    name,
    scenario: "settings-default",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    prepare: async (page) => {
      await openMain(page);
      await page.evaluate((pairs) => {
        for (const [event, payload] of pairs) {
          window.__voxshot.emit(event, payload);
        }
      }, events);
      await settle(700);
    },
  })),

  // ---- overlay states (streaming pill + minimal form) ----
  {
    name: "overlay-recording",
    scenario: "overlay",
    url: OVERLAY,
    viewport: { width: 320, height: 140 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "recording");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
      });
      await settle(1400);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
  {
    name: "overlay-minimal-recording",
    scenario: "overlay-minimal",
    url: OVERLAY,
    viewport: { width: 320, height: 140 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "recording");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
      });
      await settle(1400);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
  {
    name: "overlay-streaming-listening",
    scenario: "overlay",
    url: OVERLAY,
    viewport: { width: 340, height: 240 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "streaming");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
        window.__voxshot.emit("stream-text-event", {
          committed: "The overlay shows live text as you speak,",
          tentative: " and rewrites the tail",
        });
      });
      await settle(1500);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
  {
    name: "overlay-streaming-working",
    scenario: "overlay",
    url: OVERLAY,
    viewport: { width: 340, height: 240 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "streaming");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
        window.__voxshot.emit("stream-text-event", {
          committed: "Polishing the final transcript now",
          tentative: "",
        });
        window.__voxshot.emit("stream-phase-event", {
          phase: "working",
          kind: "polishing",
        });
      });
      await settle(1200);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
  {
    name: "overlay-notice-error",
    scenario: "overlay",
    url: OVERLAY,
    viewport: { width: 340, height: 240 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "recording");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
        window.__voxshot.emit("overlay-notice-event", {
          kind: "error",
          code: "model_fallback",
          detail: "Canary 180M Flash",
        });
      });
      await settle(900);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
  {
    name: "overlay-command-mode",
    scenario: "overlay",
    url: OVERLAY,
    viewport: { width: 340, height: 240 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "streaming");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
        window.__voxshot.emit("stream-text-event", {
          committed: "Everything you now say edits the buffer",
          tentative: " comma",
        });
        window.__voxshot.emit("command-modifier-changed", true);
      });
      await settle(900);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
  {
    name: "overlay-deleted-chip",
    scenario: "overlay",
    url: OVERLAY,
    viewport: { width: 340, height: 240 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "streaming");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
        window.__voxshot.emit("stream-text-event", {
          committed: "ship the new overlay by",
          tentative: "",
          deleted: "ignore this",
        });
      });
      await settle(700);
    },
    cleanup: async (page) =>
      page.evaluate(() => window.__voxshot.stopMicLevels()),
  },
];

// --- run ---
await mkdir(outDir, { recursive: true });
const browser = await chromium.launch({ headless: true });
const manifest = [];
const auditResults = {};
const gaps = [];

for (const shot of shots) {
  if (only && !shot.name.includes(only)) continue;
  const context = await browser.newContext({
    viewport: shot.viewport,
    deviceScaleFactor: 2,
    colorScheme: "dark",
  });
  await context.addInitScript({ path: mockPath });
  const page = await context.newPage();
  page.on("pageerror", (err) =>
    console.error(`  [pageerror:${shot.name}] ${err.message.split("\n")[0]}`),
  );
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      console.error(`  [console:${shot.name}] ${msg.text().slice(0, 160)}`);
    }
  });
  const entry = {
    name: shot.name,
    scenario: shot.scenario,
    url: shot.url,
    file: `${shot.name}.png`,
  };
  try {
    // Every screen navigates first; prepare then waits/interacts.
    await page.goto(`${base}/${shot.url}?voxshot=${shot.scenario}`, {
      waitUntil: "networkidle",
    });
    if (shot.prepare) {
      await shot.prepare(page);
    } else {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await settle(900);
    }
    await page.waitForSelector("#root div", { timeout: 5000 }).catch(() => {});
    let target = page;
    if (shot.clipTo) {
      const el = await shot.clipTo(page);
      target = el;
    }
    await target.screenshot({
      path: path.join(outDir, `${shot.name}.png`),
      animations: "disabled",
    });
    auditResults[shot.name] = await page.evaluate(auditScript);
    const unknown = await page.evaluate(
      () => window.__voxshot?.calls.unknown ?? [],
    );
    if (unknown.length) entry.unmockedCommands = unknown;
    console.log(`shot ${shot.name}`);
  } catch (error) {
    entry.error = String(error.message || error).split("\n")[0];
    gaps.push(`${shot.name}: ${entry.error}`);
    console.error(`FAIL ${shot.name}: ${entry.error}`);
    try {
      const diag = await page.evaluate(
        () =>
          `url=${location.href} rootKids=${
            document.getElementById("root")?.childElementCount ?? -1
          } mock=${Boolean(window.__voxshot)} bodyLen=${document.body.innerHTML.length}`,
      );
      console.error(`  [diag:${shot.name}] ${diag}`);
    } catch {
      console.error(`  [diag:${shot.name}] page gone`);
    }
  } finally {
    if (shot.cleanup) await shot.cleanup(page).catch(() => {});
    await context.close();
  }
  manifest.push(entry);
}

await browser.close();
server.close();

await writeFile(
  path.join(outDir, "dom-audit.json"),
  JSON.stringify(
    { generatedAt: new Date().toISOString(), screens: auditResults },
    null,
    2,
  ) + "\n",
);
await writeFile(
  path.join(outDir, "manifest.json"),
  JSON.stringify(
    { generatedAt: new Date().toISOString(), base, shots: manifest },
    null,
    2,
  ) + "\n",
);

// Keep the repo-wide prettier gate green: the generated JSON sidecars are
// committed next to the shots, so hand them to the project prettier.
try {
  const { execFile } = await import("node:child_process");
  const prettierBin = path.join(
    projectRoot,
    "node_modules",
    ".bin",
    "prettier",
  );
  await new Promise((resolve, reject) => {
    execFile(
      prettierBin,
      [
        "--write",
        path.join(outDir, "dom-audit.json"),
        path.join(outDir, "manifest.json"),
      ],
      { cwd: projectRoot },
      (error) => (error ? reject(error) : resolve()),
    );
  });
} catch (error) {
  console.warn(
    `warn: could not prettier-format generated JSON (${error.message})`,
  );
}

const failed = manifest.filter((s) => s.error);
console.log(
  `\n${manifest.length - failed.length}/${manifest.length} shots captured into ${outDir}`,
);
if (failed.length) {
  console.log("failed shots:");
  for (const f of failed) console.log(`  - ${f.name}: ${f.error}`);
}
process.exit(failed.length ? 2 : 0);
