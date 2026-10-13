// VoxBar v1.6.3 companion pipeline proof — drives the real companion page
// in real Chromium via the repo's playwright, streaming REAL recorded WAV
// speech through the page's own sendChunk path, both hold and tap modes.
//
// Run 3 (final evidence pass): tolerant of the known post-save hang —
// each leg runs to completion regardless, capturing DOM state, every WS
// frame, and screenshots, so the exact break point is documented per mode.
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const LOG = "/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log";
const OUT = "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/shots/proof-v1.6.3";
// run-2's own re-save of the 44.25s hold WAV (clean, single-stream)
const [HOLD_WAV, TAP_WAV] = pickWavs(2);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logText = fs.readFileSync(LOG, "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];
console.log(`[setup] server URL from log: ${BASE}`);

const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;
console.log(`[setup] token loaded (${token.slice(0, 6)}…, 32 hex)`);

// v1.6.3 controller patch: history cleanup deletes recordings, so the
// proof picks the newest surviving WAVs at run time instead of hardcoding.
function pickWavs(count) {
  const dir = `${APP_SUPPORT}/recordings`;
  const wavs = fs
    .readdirSync(dir)
    .filter((f) => f.endsWith(".wav"))
    .map((f) => `${dir}/${f}`)
    .sort();
  if (wavs.length < count)
    throw new Error(`need ${count} recordings, found ${wavs.length}`);
  return wavs.slice(-count);
}


function loadWavChunks(path) {
  const buf = fs.readFileSync(path);
  if (buf.toString("ascii", 0, 4) !== "RIFF" || buf.toString("ascii", 8, 12) !== "WAVE")
    throw new Error(`${path}: not RIFF/WAVE`);
  let off = 12;
  let fmt = null;
  let dataOff = -1;
  let dataLen = 0;
  while (off + 8 <= buf.length) {
    const id = buf.toString("ascii", off, off + 4);
    const size = buf.readUInt32LE(off + 4);
    if (id === "fmt ")
      fmt = {
        audioFormat: buf.readUInt16LE(off + 8),
        chans: buf.readUInt16LE(off + 10),
        rate: buf.readUInt32LE(off + 12),
        bits: buf.readUInt16LE(off + 22),
      };
    else if (id === "data") {
      dataOff = off + 8;
      dataLen = size;
      break;
    }
    off += 8 + size + (size % 2);
  }
  if (!fmt || dataOff < 0) throw new Error(`${path}: no fmt/data chunk`);
  if (fmt.audioFormat !== 1 || fmt.chans !== 1 || fmt.rate !== 16000 || fmt.bits !== 16)
    throw new Error(`${path}: expected PCM16 mono 16k, got ${JSON.stringify(fmt)}`);
  const n = Math.floor(dataLen / 2);
  const chunks = [];
  for (let i = 0; i < n; i += 640) {
    const len = Math.min(640, n - i);
    const c = new Array(len);
    for (let j = 0; j < len; j++) c[j] = buf.readInt16LE(dataOff + (i + j) * 2) / 32768;
    chunks.push(c);
  }
  console.log(
    `[setup] ${path.split("/").pop()}: ${n} samples = ${(n / 16000).toFixed(2)}s -> ${chunks.length} x 640-sample chunks`,
  );
  return chunks;
}

const holdChunks = loadWavChunks(HOLD_WAV);
const tapChunks = loadWavChunks(TAP_WAV);

const browser = await chromium.launch({
  headless: false,
  args: ["--autoplay-policy=no-user-gesture-required"],
});
const context = await browser.newContext({
  ignoreHTTPSErrors: true,
  viewport: { width: 390, height: 844 },
  locale: "en-US",
});
await context.addInitScript(() => {
  try {
    navigator.mediaDevices.getUserMedia = async () => {
      const ac = new (window.AudioContext || window.webkitAudioContext)();
      const dest = ac.createMediaStreamDestination();
      window.__proofSilentAC = ac;
      return dest.stream;
    };
  } catch (e) {}
});
const page = await context.newPage();
page.on("console", (m) => {
  if (m.type() === "error") console.log(`[page console.error] ${m.text()}`);
});
const wsFrames = [];
page.on("websocket", (ws) => {
  console.log(`[ws] opened ${ws.url()}`);
  ws.on("framereceived", (f) => {
    const s = typeof f.payload === "string" ? f.payload : `<binary ${f.payload.length}B>`;
    wsFrames.push({ t: new Date().toISOString(), dir: "recv", s });
    console.log(`[ws] recv: ${s.slice(0, 400)}`);
  });
  ws.on("close", () => {
    wsFrames.push({ t: new Date().toISOString(), dir: "close" });
    console.log("[ws] closed");
  });
});

const summary = { base: BASE, hold: {}, tap: {}, wsFrames };

// 1. open the page the way the QR does; the page's own connect() sends the
// token-authenticated hello.
await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
await page.waitForSelector("#state.ready", { timeout: 20000 });
summary.welcome = await page.evaluate(() => ({
  state: document.getElementById("stateText").textContent,
  activation,
  pttDisabled: document.getElementById("ptt").disabled,
  pttLabel: document.getElementById("ptt").textContent,
}));
console.log(`[connect] welcome: ${JSON.stringify(summary.welcome)}`);
await page.screenshot({ path: `${OUT}/01-connected.png` });

// 2. output target -> Phone
await page.click("#tgtPhone");
const targetOk = await page.evaluate(
  () => document.getElementById("tgtPhone").getAttribute("aria-pressed") === "true",
);
console.log(`[connect] output target set to Phone (aria-pressed=${targetOk})`);
summary.targetPhone = targetOk;

async function pressTalk() {
  const b = await page.locator("#ptt").boundingBox();
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await page.mouse.down();
}
async function releaseTalk() {
  await page.mouse.up();
}
async function tapTalk(ms) {
  await pressTalk();
  await sleep(ms);
  await releaseTalk();
}
async function streamWav(chunks, label) {
  const t0 = Date.now();
  for (let i = 0; i < chunks.length; i++) {
    await page.evaluate((c) => sendChunk(c), chunks[i]);
    const waitMs = t0 + (i + 1) * 40 - Date.now();
    if (waitMs > 2) await sleep(waitMs);
    if (i % 250 === 0)
      console.log(`[${label}] streamed ${((i * 640) / 16000).toFixed(1)}s of audio`);
  }
  console.log(
    `[${label}] streamed all ${chunks.length} chunks (${((chunks.length * 640) / 16000).toFixed(1)}s) in ${((Date.now() - t0) / 1000).toFixed(1)}s wall`,
  );
}
async function silenceOwnGraph() {
  await page.waitForFunction(
    () => typeof audioNode !== "undefined" && audioNode !== null,
    null,
    { timeout: 5000 },
  ).catch(() => {});
  return page.evaluate(() => {
    if (typeof audioNode === "undefined" || !audioNode) return false;
    try {
      if (audioNode.port) audioNode.port.onmessage = null;
    } catch (e) {}
    try {
      audioNode.onaudioprocess = null;
    } catch (e) {}
    try {
      audioNode.disconnect();
    } catch (e) {}
    return true;
  });
}
async function domSnapshot() {
  return page.evaluate(() => ({
    state: document.getElementById("stateText").textContent,
    stateClass: document.getElementById("state").className,
    resultShown: document.getElementById("result").classList.contains("show"),
    resultText: document.getElementById("resultText").textContent,
    meterWidth: document.querySelector("#micbar .lvl").style.width,
    meterDim: document.querySelector("#micbar .lvl").classList.contains("dim"),
    pttLabel: document.getElementById("ptt").textContent,
  }));
}
async function awaitTranscript(prevText, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const t = await page.evaluate(() => document.getElementById("resultText").textContent);
    if (t && t !== prevText && (t.match(/[A-Za-z]{2,}/g) || []).length >= 3) return t;
    await sleep(500);
  }
  return null; // caller records the miss
}
async function serverProbe() {
  return page
    .evaluate(() =>
      fetch(location.href, { cache: "no-store" })
        .then((r) => `http ${r.status}`)
        .catch((e) => `fetch failed: ${e}`),
    )
    .catch((e) => `evaluate failed: ${e}`);
}

// ============ 3. HOLD PROOF ============
console.log("[hold] pointerdown on talk button");
await pressTalk();
await page.waitForSelector("#state.recording", { timeout: 10000 });
await silenceOwnGraph();
summary.hold.recordingStart = await domSnapshot();
console.log(`[hold] Mac reports session live: ${JSON.stringify(summary.hold.recordingStart)}`);
await streamWav(holdChunks, "hold");
console.log("[hold] pointerup (release to finish)");
await releaseTalk();
await page.waitForSelector("#state.ready", { timeout: 15000 });
summary.hold.afterRelease = await domSnapshot();
console.log(`[hold] after release: ${JSON.stringify(summary.hold.afterRelease)}`);
await page.screenshot({ path: `${OUT}/02-hold-after-release.png`, fullPage: true });
const holdText = await awaitTranscript("", 75000);
summary.hold.transcriptRendered = holdText;
summary.hold.serverProbeAfter = await serverProbe();
console.log(
  holdText
    ? `[hold] TRANSCRIPT RENDERED IN PAGE: ${JSON.stringify(holdText)}`
    : `[hold] NO transcript frame reached the page within 75s; server probe: ${summary.hold.serverProbeAfter}`,
);
if (holdText)
  await page.screenshot({ path: `${OUT}/02-hold-transcript.png`, fullPage: true });

// ============ 4. TAP PROOF ============
console.log("[tap] quick tap (~150ms < 300ms hold threshold) to LOCK");
await tapTalk(150);
await page.waitForSelector("#state.recording", { timeout: 10000 });
await silenceOwnGraph();
await sleep(2000);
summary.tap.lockHeldAfter2s = await page.evaluate((c) =>
  document.getElementById("state").className.includes("recording"),
);
console.log(`[tap] session still locked 2s after release: ${summary.tap.lockHeldAfter2s}`);
summary.tap.lockedSnapshot = await domSnapshot();
await page.screenshot({ path: `${OUT}/03-tap-locked.png`, fullPage: true });
await streamWav(tapChunks, "tap");
console.log("[tap] second tap to stop");
await tapTalk(150);
await page.waitForSelector("#state.ready", { timeout: 15000 });
summary.tap.afterStop = await domSnapshot();
console.log(`[tap] after stop tap: ${JSON.stringify(summary.tap.afterStop)}`);
await page.screenshot({ path: `${OUT}/04-tap-after-stop.png`, fullPage: true });
const tapText = await awaitTranscript(holdText || "", 75000);
summary.tap.transcriptRendered = tapText;
summary.tap.serverProbeAfter = await serverProbe();
console.log(
  tapText
    ? `[tap] TRANSCRIPT RENDERED IN PAGE: ${JSON.stringify(tapText)}`
    : `[tap] NO transcript frame reached the page within 75s; server probe: ${summary.tap.serverProbeAfter}`,
);
if (tapText) await page.screenshot({ path: `${OUT}/04-tap-transcript.png`, fullPage: true });

summary.recent = await page.evaluate(() => ({
  visible: document.getElementById("recent").classList.contains("show"),
  items: [...document.querySelectorAll("#recentList .recent-text")].map((e) => e.textContent),
}));
fs.writeFileSync(`${OUT}/summary.json`, JSON.stringify(summary, null, 2));
console.log("[done] evidence pass complete");
await browser.close();
