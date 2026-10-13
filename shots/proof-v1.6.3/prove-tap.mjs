// TAP-ONLY proof leg (clean app instance): tap to lock, stream real WAV
// speech while locked, tap to stop. Documents DOM state, WS frames, and
// whether the transcript frame ever reaches the page.
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const LOG = "/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log";
const OUT = "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/shots/proof-v1.6.3";
const [TAP_WAV] = pickWavs(1);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logText = fs.readFileSync(LOG, "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;

function loadWavChunks(path) {
  const buf = fs.readFileSync(path);
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
  if (!fmt || dataOff < 0) throw new Error("no fmt/data");
  if (fmt.audioFormat !== 1 || fmt.chans !== 1 || fmt.rate !== 16000 || fmt.bits !== 16)
    throw new Error(`expected PCM16 mono 16k, got ${JSON.stringify(fmt)}`);
  const n = Math.floor(dataLen / 2);
  const chunks = [];
  for (let i = 0; i < n; i += 640) {
    const len = Math.min(640, n - i);
    const c = new Array(len);
    for (let j = 0; j < len; j++) c[j] = buf.readInt16LE(dataOff + (i + j) * 2) / 32768;
    chunks.push(c);
  }
  console.log(
    `[setup] ${path.split("/").pop()}: ${n} samples = ${(n / 16000).toFixed(2)}s -> ${chunks.length} chunks`,
  );
  return chunks;
}
const tapChunks = loadWavChunks(TAP_WAV);

const browser = await chromium.launch({
  headless: false,
  args: ["--autoplay-policy=no-user-gesture-required"],
});
const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 }, locale: "en-US" });
await context.addInitScript(() => {
  try {
    navigator.mediaDevices.getUserMedia = async () => {
      const ac = new AudioContext();
      window.__proofSilentAC = ac;
      return ac.createMediaStreamDestination().stream;
    };
  } catch (e) {}
});
const page = await context.newPage();
const wsFrames = [];
page.on("websocket", (ws) => {
  ws.on("framereceived", (f) => {
    const s = typeof f.payload === "string" ? f.payload : `<binary ${f.payload.length}B>`;
    wsFrames.push(s.slice(0, 300));
    console.log(`[ws] recv: ${s.slice(0, 400)}`);
  });
});

const summary = { base: BASE, mode: "tap" };
await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
await page.waitForSelector("#state.ready", { timeout: 20000 });
summary.welcome = await page.evaluate(() => ({
  state: document.getElementById("stateText").textContent,
  activation,
}));
console.log(`[connect] ${JSON.stringify(summary.welcome)}`);
await page.click("#tgtPhone");
console.log(`[connect] target Phone: ${await page.evaluate(() => document.getElementById("tgtPhone").getAttribute("aria-pressed"))}`);
await page.screenshot({ path: `${OUT}/05-tap-connected.png` });

async function pressTalk() {
  const b = await page.locator("#ptt").boundingBox();
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await page.mouse.down();
}
async function tapTalk(ms) {
  await pressTalk();
  await sleep(ms);
  await page.mouse.up();
}
async function silenceOwnGraph() {
  await page.waitForFunction(() => typeof audioNode !== "undefined" && audioNode !== null, null, {
    timeout: 5000,
  }).catch(() => {});
  return page.evaluate(() => {
    if (typeof audioNode === "undefined" || !audioNode) return false;
    try { if (audioNode.port) audioNode.port.onmessage = null; } catch (e) {}
    try { audioNode.onaudioprocess = null; } catch (e) {}
    try { audioNode.disconnect(); } catch (e) {}
    return true;
  });
}
const domSnap = () =>
  page.evaluate(() => ({
    state: document.getElementById("stateText").textContent,
    stateClass: document.getElementById("state").className,
    resultShown: document.getElementById("result").classList.contains("show"),
    resultText: document.getElementById("resultText").textContent,
    meterWidth: document.querySelector("#micbar .lvl").style.width,
    pttLabel: document.getElementById("ptt").textContent,
  }));

// tap #1 to LOCK (~150ms < 300ms threshold)
console.log("[tap] tap #1 to lock");
await tapTalk(150);
await page.waitForSelector("#state.recording", { timeout: 10000 });
await silenceOwnGraph();
await sleep(2000);
summary.lockHeldAfter2s = await page.evaluate((c) =>
  document.getElementById("state").className.includes("recording"),
);
summary.lockedSnapshot = await domSnap();
console.log(`[tap] locked (2s after release): ${summary.lockHeldAfter2s} ${JSON.stringify(summary.lockedSnapshot)}`);
await page.screenshot({ path: `${OUT}/03-tap-locked.png`, fullPage: true });

// stream real speech while locked
const t0 = Date.now();
for (let i = 0; i < tapChunks.length; i++) {
  await page.evaluate((c) => sendChunk(c), tapChunks[i]);
  const waitMs = t0 + (i + 1) * 40 - Date.now();
  if (waitMs > 2) await sleep(waitMs);
  if (i % 400 === 0) console.log(`[tap] streamed ${((i * 640) / 16000).toFixed(1)}s while locked`);
}
console.log(`[tap] streamed all ${tapChunks.length} chunks in ${((Date.now() - t0) / 1000).toFixed(1)}s wall`);
summary.midLockSnapshot = await domSnap();
console.log(`[tap] mid/end-of-stream DOM: ${JSON.stringify(summary.midLockSnapshot)}`);

// tap #2 to STOP
console.log("[tap] tap #2 to stop");
await tapTalk(150);
await page.waitForSelector("#state.ready", { timeout: 15000 });
summary.afterStop = await domSnap();
console.log(`[tap] after stop: ${JSON.stringify(summary.afterStop)}`);
await page.screenshot({ path: `${OUT}/04-tap-after-stop.png`, fullPage: true });

// wait for transcript
const deadline = Date.now() + 75000;
let tapText = null;
while (Date.now() < deadline) {
  const t = await page.evaluate(() => document.getElementById("resultText").textContent);
  if (t && (t.match(/[A-Za-z]{2,}/g) || []).length >= 3) { tapText = t; break; }
  await sleep(500);
}
summary.transcriptRendered = tapText;
console.log(
  tapText
    ? `[tap] TRANSCRIPT RENDERED IN PAGE: ${JSON.stringify(tapText)}`
    : "[tap] NO transcript frame reached the page within 75s",
);
if (tapText) await page.screenshot({ path: `${OUT}/04-tap-transcript.png`, fullPage: true });
summary.serverProbeAfter = await page
  .evaluate(() => fetch(location.href, { cache: "no-store" }).then((r) => `http ${r.status}`).catch((e) => `failed: ${e}`))
  .catch((e) => `evaluate failed: ${e}`);
summary.wsFrames = wsFrames;
fs.writeFileSync(`${OUT}/summary-tap.json`, JSON.stringify(summary, null, 2));
console.log(`[done] tap leg complete; server probe after: ${summary.serverProbeAfter}`);
await browser.close();

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

