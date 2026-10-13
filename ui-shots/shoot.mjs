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
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const here = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.resolve(here, "..");
const distDir = path.join(projectRoot, "dist");
const mockPath = path.join(here, "tauri-mock-init.js");
const companionMockPath = path.join(here, "companion-mock-init.js");
const companionSrcPath = path.join(
  projectRoot,
  "src-tauri",
  "src",
  "companion",
  "client",
  "index.html",
);
const localesDir = path.join(projectRoot, "src", "i18n", "locales");

// --- locale data (read once; drives the companion STRINGS injection, the
// locale-overflow pass, and localized sidebar/button clicks) ---
const localeNames = readdirSync(localesDir, { withFileTypes: true })
  .filter((e) => e.isDirectory())
  .map((e) => e.name)
  .sort();
const translations = {};
for (const lang of localeNames) {
  translations[lang] = JSON.parse(
    readFileSync(path.join(localesDir, lang, "translation.json"), "utf8"),
  );
}
const tIn = (lang, dotPath) => {
  let node = translations[lang] || translations.en;
  for (const part of dotPath.split(".")) {
    node = node?.[part];
  }
  return typeof node === "string" ? node : null;
};

// Companion STRINGS object: the exact blob the Rust server injects
// (scripts/embed-companion-strings.ts builds strings_gen.rs from the same
// companion.* keys), rebuilt here from the locale files so the rig serves
// the page byte-equivalent to production.
const companionStrings = (() => {
  const reference = translations.en.companion;
  const keys = Object.keys(reference).sort();
  const all = {};
  for (const lang of localeNames) {
    const companion = translations[lang].companion;
    if (!companion) continue;
    const ordered = {};
    for (const key of keys) ordered[key] = companion[key];
    all[lang] = ordered;
  }
  return all;
})();

// Longest-translation locale by MAX SINGLE STRING length across every
// locale file: the worst case for labels that have no room to wrap.
const longestLocale = (() => {
  const stringsOf = (obj) => {
    const out = [];
    for (const value of Object.values(obj)) {
      if (typeof value === "string") out.push(value);
      else if (value && typeof value === "object")
        out.push(...stringsOf(value));
    }
    return out;
  };
  let best = { lang: "en", max: 0 };
  for (const lang of localeNames) {
    const max = Math.max(
      0,
      ...stringsOf(translations[lang]).map((s) => s.length),
    );
    if (max > best.max) best = { lang, max };
  }
  return best;
})();
console.log(
  `locale-overflow locale: ${longestLocale.lang} (max string ${longestLocale.max} chars)`,
);

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
    // The companion phone page lives outside dist/ (it is embedded in the
    // Rust binary at release time). Serve it from source with the same
    // STRINGS injection the server performs via strings_gen.
    if (urlPath === "/companion/index.html") {
      const html = await readFile(companionSrcPath, "utf8");
      const marker = "/*__COMPANION_STRINGS__*/null";
      if (!html.includes(marker)) {
        res.writeHead(500).end("companion strings marker missing");
        return;
      }
      const injected = html.replace(marker, JSON.stringify(companionStrings));
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(injected);
      return;
    }
    // Mock recordings: the browser-side mock's convertFileSrc routes every
    // get_audio_file_path result here. A synthesized WAV with a duration
    // deterministically derived from the file name gives the history audio
    // players a REAL total length to resolve via preload="metadata" (the
    // old asset://mock/ URL could never load, so every player read 0:00).
    if (urlPath.startsWith("/mock-asset/")) {
      const raw = decodeURIComponent(urlPath.slice("/mock-asset/".length));
      const name = raw.split("/").pop() || "clip";
      let hash = 0;
      for (const ch of name) hash = (hash * 31 + ch.charCodeAt(0)) >>> 0;
      const seconds = 8 + (hash % 53); // 8..60s, stable per file name
      const sampleRate = 8000;
      const dataSize = seconds * sampleRate * 2;
      const buffer = Buffer.alloc(44 + dataSize);
      buffer.write("RIFF", 0);
      buffer.writeUInt32LE(36 + dataSize, 4);
      buffer.write("WAVE", 8);
      buffer.write("fmt ", 12);
      buffer.writeUInt32LE(16, 16);
      buffer.writeUInt16LE(1, 20); // PCM
      buffer.writeUInt16LE(1, 22); // mono
      buffer.writeUInt32LE(sampleRate, 24);
      buffer.writeUInt32LE(sampleRate * 2, 28);
      buffer.writeUInt16LE(2, 32);
      buffer.writeUInt16LE(16, 34);
      buffer.write("data", 36);
      buffer.writeUInt32LE(dataSize, 40);
      // One second of 440Hz tone up front, silence after: valid, tiny, and
      // enough for Chromium to report a real duration.
      for (let i = 0; i < sampleRate; i++) {
        buffer.writeInt16LE(
          Math.round(6000 * Math.sin((2 * Math.PI * 440 * i) / sampleRate)),
          44 + i * 2,
        );
      }
      res.writeHead(200, {
        "content-type": "audio/wav",
        "accept-ranges": "bytes",
        "content-length": buffer.length,
      });
      res.end(buffer);
      return;
    }
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
  // Buttons a screen reader would announce as just "button": no text
  // content, no aria-label/labelledby, no title. Icon-only controls are the
  // usual offenders (reset arrows, row actions), so every screen reports
  // its unnamed-button list for the a11y pass to verify against.
  const unnamedButtons = [];
  const all = Array.from(document.querySelectorAll("body *")).slice(0, 4000);

  for (const el of all) {
    if (!isVisible(el)) continue;
    const style = getComputedStyle(el);

    // Accessible-name check on interactive buttons (visible ones only).
    if (
      el.tagName === "BUTTON" &&
      !el.disabled &&
      el.getAttribute("aria-hidden") !== "true"
    ) {
      const labelledby = el.getAttribute("aria-labelledby");
      const labelledByText = labelledby
        ? labelledby
            .split(/\s+/)
            .map((id) => document.getElementById(id)?.textContent?.trim() || "")
            .join(" ")
            .trim()
        : "";
      const label =
        el.getAttribute("aria-label") ||
        el.getAttribute("title") ||
        el.textContent?.trim() ||
        labelledByText;
      if (!label) {
        unnamedButtons.push(selectorOf(el));
      }
    }

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
    unnamedButtons,
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

// Standard settings shot on a given sidebar section. `lang` picks the
// locale both for the URL override and for the sidebar title clicked.
const SECTION_SCREEN = {
  postprocessing: "post-processing",
  promptStudio: "prompt-studio",
};
// Sidebar locale keys are camelCase (sidebar.postProcessing), while shot
// section slugs are lowercase.
const SECTION_LOCALE_KEY = {
  postprocessing: "postProcessing",
  promptStudio: "promptStudio",
};
const settingsShot = ({
  name,
  scenario = "settings-default",
  section,
  lang = "en",
  theme,
  accent,
  state = "default",
  screen,
  prepare,
}) => ({
  name,
  scenario,
  url: MAIN,
  viewport: { width: 680, height: 570 },
  theme,
  accent,
  lang,
  state,
  screen:
    screen ||
    (section ? `settings/${SECTION_SCREEN[section] || section}` : undefined),
  prepare: async (page) => {
    await openMain(page);
    if (section) {
      await clickSidebar(
        page,
        tIn(lang, `sidebar.${SECTION_LOCALE_KEY[section] || section}`),
      );
    }
    if (prepare) await prepare(page);
  },
});

// Companion handshake: replay the server's first frame (the page sends
// hello on socket open, the Mac answers welcome with the activation mode
// from the app's shortcut-activation setting).
const companionHandshake = async (page, activation = "hold_or_toggle") => {
  await page.waitForFunction(
    () => window.__companion?.latest()?.readyState === 1,
    { timeout: 10000 },
  );
  await page.evaluate((mode) => {
    window.__companion.message({
      type: "welcome",
      activation: mode,
      recording: false,
    });
  }, activation);
};

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
      // Permissions are granted, so the wizard shows the All set! card,
      // which now waits for the user's Done instead of auto-advancing.
      await page.waitForSelector("#root div", { timeout: 15000 });
      await page
        .getByRole("button", {
          name: tIn("en", "onboarding.permissions.done"),
        })
        .click({ timeout: 15000 });
      await page.waitForFunction(() => document.body.innerText.length > 200, {
        timeout: 15000,
      });
      await settle(1600);
    },
  },
  // The brief all-granted card between the permission step and the model
  // step: the only place onboarding names the dictation hotkey. The card
  // auto-advances after 300ms, so the shot fires the moment its text lands.
  {
    name: "onboarding-hotkey-hint",
    scenario: "onboarding-model",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    state: "all-granted-hotkey",
    selfShot: true,
    // The all-granted card shows for only ~300ms and is gone long before
    // the default networkidle navigation settles, so this shot navigates
    // on commit and arms its wait before the wizard even mounts.
    waitUntil: "commit",
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await page.waitForFunction(
        (label) => document.body.innerText.includes(label),
        tIn("en", "onboarding.permissions.allGranted"),
        { timeout: 15000 },
      );
      await page.screenshot({
        path: path.join(outDir, "onboarding-hotkey-hint.png"),
        animations: "disabled",
      });
    },
  },
  // Model step with a dead catalog: the offline dead end (KB-186) with
  // retry and skip affordances.
  {
    name: "onboarding-model-offline",
    scenario: "onboarding-model-offline",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    state: "catalog-offline-error",
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      // The All set! card waits for Done before the (dead) model step.
      await page
        .getByRole("button", {
          name: tIn("en", "onboarding.permissions.done"),
        })
        .click({ timeout: 15000 });
      await page.waitForFunction(
        (label) => document.body.innerText.includes(label),
        tIn("en", "onboarding.modelError.title"),
        { timeout: 15000 },
      );
      await settle(800);
    },
  },
  // Model step mid-download: the card was clicked (the wizard's real
  // download gesture) and progress events stream from the backend mock.
  {
    name: "onboarding-model-downloading",
    scenario: "onboarding-model",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    state: "model-downloading",
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await page
        .getByRole("button", {
          name: tIn("en", "onboarding.permissions.done"),
        })
        .click({ timeout: 15000 });
      await page.waitForFunction(() => document.body.innerText.length > 200, {
        timeout: 15000,
      });
      await settle(1600);
      // Nemotron Streaming 3.5 is the first recommended not-yet-downloaded
      // card (Whisper Medium sits behind "Show all models"); clicking the
      // card is the wizard's real download gesture. The mock catalog's ids
      // have no locale entries, so the card shows the catalog name.
      await page
        .getByText("Nemotron Streaming 3.5", { exact: true })
        .first()
        .click({ timeout: 5000 });
      await settle(300);
      await page.evaluate(() => {
        window.__voxshot.emit("model-download-progress", {
          model_id:
            "handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
          downloaded: 226000000,
          total: 679477248,
          percentage: 33.3,
        });
      });
      // The card's action row (progress bar + cancel) sits below the fold at
      // the resting scroll; the progress caption is the row's own text, so
      // scrolling IT into view frames the in-flight state as the subject
      // instead of a cropped card edge.
      await page
        .getByText(tIn("en", "modelSelector.downloading").replace("{{percentage}}", "33"), { exact: false })
        .last()
        .scrollIntoViewIfNeeded({ timeout: 5000 });
      await settle(900);
    },
  },
  // Model step mid-verification: the download reached 100% and the backend
  // began the sha256 pass, so the card shows the verifying spinner (the
  // between-download-and-ready state the downloading shot cannot frame).
  {
    name: "onboarding-model-verifying",
    scenario: "onboarding-model",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    state: "model-verifying",
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await page
        .getByRole("button", {
          name: tIn("en", "onboarding.permissions.done"),
        })
        .click({ timeout: 15000 });
      await page.waitForFunction(() => document.body.innerText.length > 200, {
        timeout: 15000,
      });
      await settle(1600);
      // Same real download gesture as the downloading shot, then the store's
      // own model-verification-started listener flips the card to verifying.
      await page
        .getByText("Nemotron Streaming 3.5", { exact: true })
        .first()
        .click({ timeout: 5000 });
      await settle(300);
      await page.evaluate(() => {
        window.__voxshot.emit("model-download-progress", {
          model_id:
            "handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
          downloaded: 679477248,
          total: 679477248,
          percentage: 100,
        });
        window.__voxshot.emit(
          "model-verification-started",
          "handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
        );
      });
      // The verifying bar renders on the card's action row below the fold;
      // scrolling the caption itself into view frames the state (the title
      // stays already-in-view, so scrolling the card would be a no-op).
      await page.waitForFunction(
        (label) => document.body.innerText.includes(label),
        tIn("en", "modelSelector.verifyingGeneric"),
        { timeout: 5000 },
      );
      await page
        .getByText(tIn("en", "modelSelector.verifyingGeneric"), { exact: true })
        .last()
        .scrollIntoViewIfNeeded({ timeout: 5000 });
      await settle(400);
    },
  },
  // First-run waiting state: the TCC permission checks have not answered
  // yet, so onboarding renders its lone centered spinner (the frame the
  // fresh/mic shots race through before first paint settles).
  {
    name: "onboarding-permissions-waiting",
    scenario: "onboarding-permissions-pending",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    state: "permissions-pending",
    prepare: async (page) => {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await settle(1200);
    },
  },

  // ---- settings tabs (default user: debug off, post-process off) ----
  ...[
    ["settings-general", "general"],
    ["settings-commands", "commands"],
    ["settings-history", "history"],
    ["settings-notifications", "notifications"],
    ["settings-models", "models"],
    ["settings-output", "output"],
    ["settings-advanced", "advanced"],
    ["settings-about", "about"],
  ].map(([name, section]) => settingsShot({ name, section })),

  // ---- Models: the ACTIVE model card's action row in view. The resting
  // tab shot keeps the first card clipped at the fold, so the destructive
  // Delete (error red, disabled while active, behind the divider) needs
  // its own scrolled capture to be visibly in evidence.
  settingsShot({
    name: "settings-models-active-delete",
    section: "models",
    state: "active-delete-row",
    prepare: async (page) => {
      await page
        .locator("div.rounded-xl")
        .filter({ hasText: tIn("en", "modelSelector.active") })
        .first()
        .scrollIntoViewIfNeeded({ timeout: 5000 });
      await settle(500);
    },
  }),

  // ---- Commands: the reset-all confirmation dialog open, capturing the
  // danger (solid error-red) confirm button against the cancel ghost.
  settingsShot({
    name: "settings-commands-reset-dialog",
    section: "commands",
    state: "reset-dialog",
    prepare: async (page) => {
      await page
        .getByRole("button", {
          name: tIn("en", "settings.commands.resetAll"),
        })
        .click({ timeout: 5000 });
      await settle(600);
    },
  }),

  // ---- disabled-control pass: the behavior toggles that gate the general
  // tab's shortcut rows are off, so those rows render disabled ----
  settingsShot({
    name: "settings-general-shortcuts-disabled",
    scenario: "settings-gated-off",
    section: "general",
    state: "disabled-controls",
  }),

  // ---- Advanced with the Experimental group on: keyboard implementation,
  // acceleration, lazy stream close, VAD backend, companion devices ----
  settingsShot({
    name: "settings-advanced-experimental",
    scenario: "settings-experimental",
    section: "advanced",
    state: "experimental-on",
  }),
  settingsShot({
    name: "settings-advanced-companion-devices",
    scenario: "settings-companion",
    section: "advanced",
    state: "companion-paired",
    prepare: async (page) => {
      // The companion devices panel sits at the bottom of the Experimental
      // group; bring it into the viewport.
      await page.evaluate(() => {
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop = scroller.scrollHeight;
      });
      await settle(600);
    },
  }),
  // The pairing QR block: the devices shot above scrolls past it to the
  // interface list, so the QR itself (title, code, hint) never rendered in
  // any capture. Framed on its own via the panel's role=img QR container.
  settingsShot({
    name: "settings-advanced-companion-qr",
    scenario: "settings-companion",
    section: "advanced",
    state: "companion-qr",
    prepare: async (page) => {
      const qrTitle = tIn("en", "settings.advanced.companionDevices.qrTitle");
      await page
        .getByText(qrTitle, { exact: true })
        .scrollIntoViewIfNeeded({ timeout: 5000 });
      await page.evaluate(() => {
        // Back the QR up a little so the title above and hint below are
        // both in frame, then let the pane settle.
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop -= 60;
      });
      await settle(600);
    },
  }),

  // ---- gated tabs (debug + post-process enabled) ----
  ...[
    ["settings-postprocessing", "postprocessing"],
    ["settings-debug", "debug"],
  ].map(([name, section]) =>
    settingsShot({ name, scenario: "settings-full", section }),
  ),

  // ---- Prompt Studio (v1.6.0 W1-L6: always enabled) ----
  settingsShot({ name: "settings-studio", section: "promptStudio" }),
  // Engines grid scrolled into view with OpenRouter carrying no model
  // (mock default): the "No model set. Choose one in Post Process." note
  // wraps to a second line instead of ellipsizing mid-destination.
  settingsShot({
    name: "settings-studio-engines-no-model",
    section: "promptStudio",
    state: "engines-no-model",
    prepare: async (page) => {
      const enginesLabel = tIn("en", "settings.studio.engines.label");
      await page
        .locator("h2, h3, p", { hasText: enginesLabel })
        .last()
        .scrollIntoViewIfNeeded({ timeout: 5000 });
      await page.evaluate(() => {
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop -= 40;
      });
      await settle(500);
    },
  }),
  settingsShot({
    name: "settings-studio-run",
    scenario: "settings-full",
    section: "promptStudio",
    state: "run-results",
    prepare: async (page) => {
      const runLabel = tIn("en", "settings.studio.run_one");
      await page.getByRole("button", { name: runLabel }).click({
        timeout: 5000,
      });
      // Let the mocked batch resolve, then bring the result cards (they
      // render below the run controls) into the viewport.
      await settle(1400);
      await page.evaluate(() => {
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop = scroller.scrollHeight;
      });
      await settle(400);
    },
  }),
  settingsShot({
    name: "settings-studio-error",
    scenario: "studio-error",
    section: "promptStudio",
    state: "error",
    prepare: async (page) => {
      const runLabel = tIn("en", "settings.studio.run_one");
      await page.getByRole("button", { name: runLabel }).click({
        timeout: 5000,
      });
      await settle(1000);
      await page.evaluate(() => {
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop = scroller.scrollHeight;
      });
      await settle(400);
    },
  }),

  // ---- Studio partial failure in LIGHT theme: both warning-text surfaces
  // (the kept-raw card's amber caption and the partial-failure Alert body)
  // on the light palette, where the old warning token measured 3.08:1.
  settingsShot({
    name: "theme-light-studio-warning",
    scenario: "studio-mixed",
    section: "promptStudio",
    theme: "light",
    state: "theme-light-warning",
    prepare: async (page) => {
      // Second engine so the batch mixes one applied + one kept-raw run.
      await page.getByText("OpenAI", { exact: true }).first().click({
        timeout: 5000,
      });
      const runLabel = tIn("en", "settings.studio.run_other").replace(
        "{{count}}",
        "2",
      );
      await page.getByRole("button", { name: runLabel }).click({
        timeout: 5000,
      });
      await settle(1400);
      await page.evaluate(() => {
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop = scroller.scrollHeight;
      });
      await settle(400);
    },
  }),

  // ---- Post Process tab on a cloud provider (model selection surface) ----
  settingsShot({
    name: "settings-postprocessing-cloud",
    scenario: "settings-pp-cloud",
    section: "postprocessing",
    state: "cloud-provider",
    prepare: async (page) => {
      // Let the on-open model-list fetch resolve and render.
      await settle(900);
    },
  }),
  // Cloud provider with no key: the picker must say no list exists rather
  // than claiming a fetch (the honest-summary state).
  settingsShot({
    name: "settings-postprocessing-cloud-nokey",
    scenario: "settings-pp-cloud-nokey",
    section: "postprocessing",
    state: "no-api-key",
    prepare: async (page) => {
      await settle(900);
    },
  }),

  // ---- Post Process prompt sub-views (list rows, editor, create form,
  // and the local model catalog grid) ----
  settingsShot({
    name: "settings-postprocessing-prompt-editor",
    scenario: "settings-full",
    section: "postprocessing",
    state: "prompt-editor",
    prepare: async (page) => {
      await page
        .getByRole("button", {
          name: tIn("en", "settings.postProcessing.prompts.editPrompt"),
        })
        .first()
        .click({ timeout: 5000 });
      await settle(700);
    },
  }),
  settingsShot({
    name: "settings-postprocessing-prompt-create",
    scenario: "settings-full",
    section: "postprocessing",
    state: "prompt-create",
    prepare: async (page) => {
      await page
        .getByRole("button", {
          name: tIn("en", "settings.postProcessing.prompts.createNew"),
        })
        .first()
        .click({ timeout: 5000 });
      await settle(700);
    },
  }),
  settingsShot({
    name: "settings-postprocessing-local-catalog",
    scenario: "settings-full",
    section: "postprocessing",
    state: "local-model-catalog",
    prepare: async (page) => {
      await page
        .getByRole("button", {
          name: tIn("en", "settings.postProcessing.models.browseCatalog"),
        })
        .first()
        .click({ timeout: 5000 });
      await settle(900);
    },
  }),

  // ---- Debug tab sub-states: onboarding preview surface + populated runs
  // table and live log (events emitted from the rig) ----
  settingsShot({
    name: "settings-debug-onboarding-preview",
    scenario: "settings-full",
    section: "debug",
    state: "onboarding-preview",
    prepare: async (page) => {
      await page
        .getByRole("button", {
          name: tIn("en", "settings.debug.onboardingPreview.permissionsButton"),
        })
        .first()
        .click({ timeout: 5000 });
      await settle(900);
    },
  }),
  settingsShot({
    name: "settings-debug-live-log",
    scenario: "settings-full",
    section: "debug",
    state: "runs-and-live-log",
    prepare: async (page) => {
      await page.evaluate(() => {
        const lines = [
          {
            level: "info",
            message:
              "pp: requested binding=transcribe_with_post_process engine=local",
          },
          {
            level: "info",
            message: "pp: engine model_load_ms=812 swap_wait_ms=40",
          },
          { level: "info", message: "pp: generation ms=1830 retries=0" },
          {
            level: "info",
            message:
              "pp: outcome=applied chars_in=214 chars_out=198 total_ms=2712",
          },
          {
            level: "warn",
            message: "memory gate: 2.4 GB free, model needs 3.1 GB",
          },
        ];
        for (const payload of lines) {
          window.__voxshot.emit("log://log", payload);
        }
      });
      await settle(900);
      // The live log viewer is the section's last row; bring it into view.
      await page.evaluate(() => {
        const scroller = document.querySelector("[data-settings-scroll]");
        if (scroller) scroller.scrollTop = scroller.scrollHeight;
      });
      await settle(400);
    },
  }),

  // ---- secure-input banner above the settings content ----
  settingsShot({
    name: "secure-input-warning",
    scenario: "settings-default",
    state: "secure-input-banner",
    prepare: async (page) => {
      await page.evaluate(() => {
        window.__voxshot.emit("secure-input-changed", {
          enabled: true,
          sustained: true,
          culprit_pid: 421,
          culprit_name: "Keynote",
          fallback_active: false,
          covered_bindings: [],
          degraded_bindings: [],
          uncovered_bindings: ["transcribe"],
          recorder_blocked: false,
        });
      });
      await settle(900);
    },
  }),

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

  // ---- shared Dropdown open with its selected row visible (light theme:
  // the selected-row fill/label contrast pair). The History tab's
  // retention dropdown sits at the top of the pane; its trigger shows the
  // current value ("Keep latest 5" under the default mock settings), and
  // opening it renders one selected row among the unselected ones.
  {
    name: "dropdown-selected-light",
    scenario: "settings-default",
    url: MAIN,
    viewport: { width: 680, height: 570 },
    theme: "light",
    state: "theme-light",
    prepare: async (page) => {
      await openMain(page);
      await clickSidebar(page, tIn("en", "sidebar.history"));
      // The trigger's label is the selected option's localized text; the
      // default mock keeps "preserve_limit" with history_limit 5.
      const triggerLabel = tIn(
        "en",
        "settings.debug.recordingRetention.preserveLimit",
      ).replace("{{count}}", "5");
      await page
        .locator("button", { hasText: triggerLabel })
        .first()
        .click({ timeout: 5000 });
      await settle(400);
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

  // ---- history empty + loading states ----
  settingsShot({
    name: "settings-history-empty",
    scenario: "history-empty",
    section: "history",
    state: "empty",
  }),
  settingsShot({
    name: "settings-history-loading",
    scenario: "history-pending",
    section: "history",
    state: "loading",
  }),

  // ---- theme and accent sweep ----
  // The main settings sections under the forced light and dark palettes
  // (AppSettings.theme), plus two accent swatches on top of each. The
  // companion phone page ships a fixed dark palette only, so its theme
  // sweep row is the default render below (see not_capturable note).
  ...["light", "dark"].flatMap((theme) =>
    ["general", "postprocessing", "history", "about"].map((section) => {
      // The app's default theme is System, which resolves dark under this
      // rig, so a dark row with the resting shot's own scenario renders
      // byte-identical pixels to that shot (the audit's duplicate md5).
      // Each dark row pairs with something its resting shot does not
      // carry, so the sweep proves the explicit dark token AND adds a
      // state no other capture holds; light rows already differ from the
      // dark resting shots and keep the plain pairing.
      const overrides =
        theme === "dark"
          ? {
              // pp-on General: the sidebar's enabled Post Process row and
              // the footer's polishing status, in explicit dark.
              general: { scenario: "settings-full" },
              // No dark-accent Post Process shot exists (the accent sweep
              // covers light only): dark + blue here is new coverage.
              postprocessing: { accent: "blue" },
              // pp-on History: entries carry their post-process badges.
              history: { scenario: "settings-full" },
              // No accent sweep touches About: dark + blue is new.
              about: { accent: "blue" },
            }[section] || {}
          : {};
      return settingsShot({
        name: `theme-${theme}-settings-${section}`,
        scenario:
          section === "postprocessing" ? "settings-full" : "settings-default",
        section,
        theme,
        state: `theme-${theme}`,
        ...overrides,
      });
    }),
  ),
  ...["blue", "violet"].flatMap((accent) =>
    ["light", "dark"].map((theme) =>
      settingsShot({
        name: `accent-${accent}-${theme}-settings-general`,
        section: "general",
        theme,
        accent,
        state: `accent-${accent}`,
      }),
    ),
  ),
  ...["blue", "violet"].map((accent) =>
    settingsShot({
      name: `accent-${accent}-light-settings-postprocessing`,
      scenario: "settings-full",
      section: "postprocessing",
      theme: "light",
      accent,
      state: `accent-${accent}`,
    }),
  ),

  // ---- locale-overflow pass ----
  // The three densest screens re-rendered in the locale with the longest
  // single translated string (computed from src/i18n/locales at startup).
  ...["general", "commands", "postprocessing"].map((section) =>
    settingsShot({
      name: `locale-${longestLocale.lang}-settings-${section}`,
      scenario:
        section === "postprocessing" ? "settings-full" : "settings-default",
      section,
      lang: longestLocale.lang,
      state: "locale-overflow",
    }),
  ),

  // ---- CJK locale pass (zh-Hans) ----
  // No capture had ever rendered a CJK glyph set: font fallback, full-width
  // punctuation and the wider line metrics only show under a real CJK
  // locale. General carries the densest mixed content; Commands also shows
  // the matrix's built-in CJK trigger aliases (句号 etc.) among the chips.
  ...["general", "commands"].map((section) =>
    settingsShot({
      name: `locale-zh-settings-${section}`,
      scenario: "settings-default",
      section,
      lang: "zh",
      state: "locale-zh",
    }),
  ),
  // ---- Devanagari locale pass (hi) ----
  // The other script family the finding names: conjunct stacks and matra
  // marks stress line height and font fallback in ways Latin/CJK do not.
  // General renders the densest mixed content (sidebar, rows, shortcut
  // chips) in Devanagari.
  settingsShot({
    name: "locale-hi-settings-general",
    scenario: "settings-default",
    section: "general",
    lang: "hi",
    state: "locale-hi",
  }),

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
        // prettier-ignore
        ["overlay-notice-event", { kind: "info", code: "command_mode_no_session", card_visible: false }],
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
    // The minimal style's real window: Rust opens the compact overlay at
    // 256x96 (overlay.rs OVERLAY_WIDTH/HEIGHT) and never the stream panel,
    // so this shot frames the pill at its native size. The style itself is
    // applied window-side (the page never reads overlay_style), which is
    // why the old 320x140 framing rendered byte-identical to
    // overlay-recording above.
    viewport: { width: 256, height: 96 },
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
    // The native streaming window (overlay.rs OVERLAY_STREAM_* = 400x168):
    // every streaming-state card shot frames the window the app opens.
    viewport: { width: 400, height: 168 },
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
    viewport: { width: 400, height: 168 },
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
    // The native streaming window (overlay.rs: OVERLAY_STREAM_* = 400x168):
    // the old 340x240 capture framed a window size the app never opens.
    viewport: { width: 400, height: 168 },
    prepare: async (page) => {
      await page.waitForFunction(() => Boolean(window.__voxshot), {
        timeout: 10000,
      });
      await settle(800);
      await page.evaluate(() => {
        window.__voxshot.emit("show-overlay", "streaming");
        window.__voxshot.emit("recording-ready", null);
        window.__voxshot.startMicLevels();
        // A real error code: the backend classifies model_fallback as
        // kind=error too, but transcription_failed is the unambiguous
        // failure the error tone exists for.
        window.__voxshot.emit("overlay-notice-event", {
          kind: "error",
          code: "transcription_failed",
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
    viewport: { width: 400, height: 168 },
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
    viewport: { width: 400, height: 168 },
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

  // ---- companion phone page (390x844) ----
  // The standalone HTML the Mac serves over its LAN TLS server. The rig
  // injects the companion.* STRINGS exactly like strings_gen does, fakes
  // the WebSocket, and drives Chromium's fake media device so the page's
  // own capture graph feeds the level meter. Server frames arrive through
  // window.__companion.message(), the fake socket's delivery path.
  // companionHandshake replays the server's first frame pair: the page
  // sends hello on open, the Mac answers welcome (activation mode from
  // the app's shortcut-activation setting).
  ...[
    {
      name: "companion-ready",
      scenario: "companion-ready",
      state: "ready",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateReady"),
          { timeout: 10000 },
        );
        await settle(600);
      },
    },
    {
      name: "companion-recording",
      scenario: "companion-ready",
      state: "recording-meter-active",
      selfShot: true,
      prepare: async (page) => {
        await companionHandshake(page);
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateReady"),
          { timeout: 10000 },
        );
        await page.evaluate(() => {
          window.__companion.message({ type: "state", recording: true });
        });
        // A real press gesture starts the page's AudioContext and capture
        // graph; Chromium's fake device feeds it samples, so the meter
        // fills through the page's own level code. The fake tone alternates
        // tone and silence bursts, so poll and capture inside a filled
        // window; if the meter never fills the shot still lands (a dead
        // meter stays visible rather than hidden).
        await page.locator("#ptt").dispatchEvent("pointerdown");
        const deadline = Date.now() + 12000;
        let captured = false;
        while (Date.now() < deadline) {
          const filled = await page.evaluate(() => {
            const el = document.querySelector("#micbar .lvl");
            return el ? parseFloat(el.style.width) : 0;
          });
          if (filled >= 25) {
            await page.screenshot({
              path: path.join(outDir, "companion-recording.png"),
              animations: "disabled",
            });
            captured = true;
            break;
          }
          await settle(60);
        }
        if (!captured) {
          await page.screenshot({
            path: path.join(outDir, "companion-recording.png"),
            animations: "disabled",
          });
        }
      },
      cleanup: async (page) => {
        await page
          .locator("#ptt")
          .dispatchEvent("pointerup")
          .catch(() => {});
      },
    },
    {
      name: "companion-mic-error",
      scenario: "companion-mic-error",
      state: "mic-permission-error",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.waitForSelector("#micError.show", { timeout: 10000 });
        await settle(500);
      },
    },
    {
      name: "companion-transcript-copy",
      scenario: "companion-ready",
      state: "transcript-result",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.evaluate(() => {
          window.__companion.message({
            type: "transcript",
            text: "Pencils down on the companion page review. The layout survives narrow screens and the copy button flashes its confirmation.",
            postProcessed: true,
            model: "Canary 180M Flash",
          });
        });
        await page.waitForSelector("#result.show", { timeout: 5000 });
        // The copy click is the page's own flash path (Copy -> Copied).
        await page.locator("#resultCopy").click({ timeout: 5000 });
        await settle(250);
      },
    },
    {
      name: "companion-recents",
      scenario: "companion-ready",
      state: "recents-populated",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.evaluate(() => {
          const now = Math.floor(Date.now() / 1000);
          window.__companion.message({
            type: "recentHistory",
            entries: [
              {
                text: "Ship the wall redesign behind the feature flag and watch the stall counter for a day.",
                timestamp: now - 240,
              },
              {
                text: "Reminder: the companion QR pairs only while the server is running.",
                timestamp: now - 7200,
              },
              {
                text: "Standup notes: the runs table now names the failure class next to every skipped polish.",
                timestamp: now - 86400,
              },
              {
                text: "buy oat milk, coffee beans, sourdough",
                timestamp: now - 172800,
              },
            ],
          });
        });
        await page.waitForSelector("#recent.show", { timeout: 5000 });
        await settle(500);
      },
    },
    {
      name: "companion-toggles-phone-pp-on",
      scenario: "companion-ready",
      state: "toggles-phone-pp-on",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateReady"),
          { timeout: 10000 },
        );
        // Real taps on the page's own controls: output target -> To Phone,
        // polish toggle -> on (both persist through the page's localStorage).
        await page.locator("#tgtPhone").click();
        await page.locator("#ppToggle").click();
        await settle(400);
      },
    },
    {
      name: "companion-error-bad-token",
      scenario: "companion-ready",
      state: "pairing-error",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.evaluate(() => {
          window.__companion.message({ type: "error", code: "bad_token" });
        });
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateError"),
          { timeout: 5000 },
        );
        await settle(400);
      },
    },
    // The pre-open state: the phone loaded the page and is waiting on the
    // WebSocket handshake (the mock's companion-connecting scenario never
    // fires onopen, so the Connecting pill + disabled button stay framed).
    {
      name: "companion-connecting",
      scenario: "companion-connecting",
      state: "connecting",
      prepare: async (page) => {
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateConnecting"),
          { timeout: 10000 },
        );
        await settle(600);
      },
    },
    // A live pairing that lost the socket: after welcome/ready the fake
    // socket drops (the Mac left the LAN), firing the page's own onclose
    // path - warn pill, disabled controls, Reconnect surfaced.
    {
      name: "companion-disconnected",
      scenario: "companion-ready",
      state: "disconnected",
      prepare: async (page) => {
        await companionHandshake(page);
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateReady"),
          { timeout: 10000 },
        );
        await page.evaluate(() => window.__companion.drop());
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("en", "companion.stateDisconnected"),
          { timeout: 5000 },
        );
        await settle(400);
      },
    },
    // The same ready surface under it-IT strings: the page resolves its
    // locale from navigator.languages, so the context carries the locale
    // and every label (pill, button, caption, footnote) renders translated.
    {
      name: "companion-it",
      scenario: "companion-ready",
      state: "ready-it",
      contextOptions: { locale: "it-IT" },
      prepare: async (page) => {
        await companionHandshake(page);
        await page.waitForFunction(
          (label) =>
            document.getElementById("stateText")?.textContent === label,
          tIn("it", "companion.stateReady"),
          { timeout: 10000 },
        );
        await settle(600);
      },
    },
  ].map((shot) => ({
    // The pairing token the page requires: any 32 hex chars, exactly the
    // format the QR URL carries (#t=<token>).
    url: `companion/index.html?voxshot=${shot.scenario}#t=0123456789abcdef0123456789abcdef`,
    viewport: { width: 390, height: 844 },
    kind: "companion",
    ...shot,
  })),
];

// Manifest metadata for the pre-existing shot families (the newer defs
// carry their own screen/state fields).
for (const s of shots) {
  if (s.url === OVERLAY) {
    s.screen = s.screen || "overlay";
    s.state = s.state || s.name.replace("overlay-", "");
  } else if (s.name.startsWith("toast-")) {
    s.screen = s.screen || "main/toast";
    s.state = s.state || s.name.replace("toast-", "");
  } else if (s.name === "footer") {
    s.screen = s.screen || "footer";
  } else if (s.name === "whats-new-modal") {
    s.screen = s.screen || "whats-new";
    s.state = s.state || "modal";
  } else if (s.name === "settings-search-dropdown") {
    s.screen = s.screen || "settings/search";
    s.state = s.state || "dropdown-open";
  } else if (s.name.startsWith("onboarding-")) {
    s.screen = s.screen || "onboarding";
    s.state = s.state || s.name.replace("onboarding-", "");
  } else if (s.name.startsWith("companion-")) {
    s.screen = s.screen || "companion";
  } else if (s.name === "secure-input-warning") {
    s.screen = s.screen || "main/secure-input";
  }
}

// --- run ---
await mkdir(outDir, { recursive: true });
const browser = await chromium.launch({
  headless: true,
  // Chromium's fake media plumbing: the companion page's getUserMedia
  // resolves against a synthetic audio device (a live tone), so its own
  // capture graph and level meter run for real inside the rig.
  args: [
    "--use-fake-device-for-media-stream",
    "--use-fake-ui-for-media-stream",
    "--autoplay-policy=no-user-gesture-required",
  ],
});
const manifest = [];
const auditResults = {};
const gaps = [];
const report = {
  generatedAt: "",
  base: "",
  localeOverflow: longestLocale,
  shots: [],
};

const viewportLabel = (shot) => {
  const kind =
    shot.kind === "companion"
      ? "mobile"
      : shot.url === OVERLAY
        ? "overlay"
        : "desktop";
  return `${kind}-${shot.viewport.width}x${shot.viewport.height}`;
};

for (const shot of shots) {
  if (only && !shot.name.includes(only)) continue;
  const isCompanion = shot.kind === "companion";
  const context = await browser.newContext({
    viewport: shot.viewport,
    deviceScaleFactor: 2,
    colorScheme: "dark",
    ...(isCompanion ? { isMobile: true, hasTouch: true } : {}),
    ...(shot.contextOptions || {}),
  });
  await context.addInitScript({
    path: isCompanion ? companionMockPath : mockPath,
  });
  const page = await context.newPage();
  if (isCompanion) {
    // The page's Copy buttons write the transcript through the clipboard
    // API; headless Chromium needs the permission granted to accept it.
    await context
      .grantPermissions(["clipboard-read", "clipboard-write"], {
        origin: base,
      })
      .catch(() => {});
  }
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
    // Every screen navigates first; prepare then waits/interacts. Shots
    // whose URL already carries its query (the companion page) are used
    // verbatim; the rest get the scenario plus optional theme/accent/lang
    // overrides, which the mock folds into the settings fixture.
    let navigateUrl;
    if (shot.url.includes("voxshot=")) {
      navigateUrl = `${base}/${shot.url}`;
    } else {
      const urlParams = new URLSearchParams({ voxshot: shot.scenario });
      if (shot.theme) urlParams.set("theme", shot.theme);
      if (shot.accent) urlParams.set("accent", shot.accent);
      if (shot.lang) urlParams.set("lang", shot.lang);
      navigateUrl = `${base}/${shot.url}?${urlParams.toString()}`;
    }
    await page.goto(navigateUrl, {
      waitUntil: shot.waitUntil || "networkidle",
    });
    if (shot.prepare) {
      await shot.prepare(page);
    } else {
      await page.waitForSelector("#root div", { timeout: 15000 });
      await settle(900);
    }
    // The companion page is plain HTML without the app's #root shell.
    if (!isCompanion) {
      await page
        .waitForSelector("#root div", { timeout: 5000 })
        .catch(() => {});
    }
    // Park the synthetic pointer at the window corner. prepare() clicks
    // leave the cursor sitting on the last target, so its :hover state
    // (and any hover-only affordance it lights up) would bleed into the
    // capture and read as a rendering artifact.
    await page.mouse.move(0, 0).catch(() => {});
    await settle(120);
    let target = page;
    if (shot.clipTo) {
      const el = await shot.clipTo(page);
      target = el;
    }
    if (!shot.selfShot) {
      await target.screenshot({
        path: path.join(outDir, `${shot.name}.png`),
        animations: "disabled",
      });
    }
    auditResults[shot.name] = await page.evaluate(auditScript);
    const unknown = await page.evaluate(
      () => window.__voxshot?.calls.unknown ?? [],
    );
    if (unknown.length) entry.unmockedCommands = unknown;
    const manifestEntry = {
      file: path.join(outDir, `${shot.name}.png`),
      screen:
        shot.screen ||
        (shot.name.startsWith("settings-")
          ? `settings/${shot.name.slice("settings-".length)}`
          : shot.name),
      state: shot.state || "default",
      theme: shot.theme || "dark",
      accent: shot.accent || "pink",
      viewport: viewportLabel(shot),
    };
    manifest.push(manifestEntry);
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
  report.shots.push(entry);
}

await browser.close();
server.close();

// Audit sidecar: per-screen DOM metrics next to the shots.
await writeFile(
  path.join(outDir, "dom-audit.json"),
  JSON.stringify(
    { generatedAt: new Date().toISOString(), screens: auditResults },
    null,
    2,
  ) + "\n",
);
// manifest.json is the capture manifest: one row per captured PNG with the
// surface, state variant, rendered theme, accent, and viewport label.
await writeFile(
  path.join(outDir, "manifest.json"),
  JSON.stringify(manifest, null, 2) + "\n",
);
// shoot-report.json keeps the rig-internal detail the manifest does not
// carry: scenario per shot, unmocked commands, and per-shot failures.
report.generatedAt = new Date().toISOString();
report.base = base;
await writeFile(
  path.join(outDir, "shoot-report.json"),
  JSON.stringify(report, null, 2) + "\n",
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
        path.join(outDir, "shoot-report.json"),
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

const failed = report.shots.filter((s) => s.error);
console.log(
  `\n${report.shots.length - failed.length}/${report.shots.length} shots captured into ${outDir}`,
);
if (failed.length) {
  console.log("failed shots:");
  for (const f of failed) console.log(`  - ${f.name}: ${f.error}`);
}
process.exit(failed.length ? 2 : 0);
