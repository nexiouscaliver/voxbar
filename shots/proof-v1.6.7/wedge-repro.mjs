// v1.6.7 wedge repro: one hold-mode phone-target session against the LIVE
// app, exactly the operator's failing shape. When the outcome logs and the
// pipeline never drains (no "Unregistered handy-keys shortcut: cancel"),
// we sample the process so the lock holder's stack names the seam.
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";
import { execSync } from "node:child_process";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const LOG = "/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log";
const OUT = "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/shots/proof-v1.6.7";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logText = fs.readFileSync(LOG, "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;
console.log(`[setup] base=${BASE} token=${token.slice(0, 6)}...`);

// newest surviving recording (history cleanup deletes older WAVs)
const dir = `${APP_SUPPORT}/recordings`;
const WAV = fs
  .readdirSync(dir)
  .filter((f) => f.endsWith(".wav"))
  .map((f) => `${dir}/${f}`)
  .sort()
  .slice(-1)[0];
if (!WAV) throw new Error("no recordings to stream");
console.log(`[setup] wav=${WAV.split("/").pop()}`);

function loadWavChunks(path, maxSecs) {
  const buf = fs.readFileSync(path);
  let off = 12;
  let dataOff = -1;
  let dataLen = 0;
  while (off + 8 <= buf.length) {
    const id = buf.toString("ascii", off, off + 4);
    const size = buf.readUInt32LE(off + 4);
    if (id === "data") {
      dataOff = off + 8;
      dataLen = size;
      break;
    }
    off += 8 + size + (size % 2);
  }
  const n = Math.min(Math.floor(dataLen / 2), maxSecs * 16000);
  const chunks = [];
  for (let i = 0; i < n; i += 640) {
    const len = Math.min(640, n - i);
    const c = new Array(len);
    for (let j = 0; j < len; j++) c[j] = buf.readInt16LE(dataOff + (i + j) * 2) / 32768;
    chunks.push(c);
  }
  return chunks;
}

const chunks = loadWavChunks(WAV, 5);
console.log(`[setup] ${(chunks.length * 640 / 16000).toFixed(2)}s of audio`);

const pid = parseInt(execSync("pgrep -x voxbar | head -1").toString().trim(), 10);
console.log(`[setup] live voxbar pid=${pid}`);

const beforeLines = fs.readFileSync(LOG, "utf8").split("\n").length;

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
      return ac.createMediaStreamDestination().stream;
    };
  } catch (e) {}
});
const page = await context.newPage();
let gotTranscript = false;
let wsSends = 0;
page.on("websocket", (ws) => {
  ws.on("framereceived", (f) => {
    const s = typeof f.payload === "string" ? f.payload : `<binary>`;
    if (s.includes("recentHistory") && s.includes('"text"')) {
      gotTranscript = true;
      console.log(`[ws] TRANSCRIPT FRAME: ${s.slice(0, 120)}`);
    }
  });
  ws.on("framesent", () => wsSends++);
});

await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
await page.waitForSelector("#state.ready", { timeout: 20000 });
console.log(`[connect] paired`);
await page.click("#tgtPhone");
console.log(`[connect] target=phone`);

// hold-to-talk: press, stream, release
const b = await page.locator("#ptt").boundingBox();
await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
await page.mouse.down();
const t0 = Date.now();
for (let i = 0; i < chunks.length; i++) {
  await page.evaluate((c) => sendChunk(c), chunks[i]);
  const waitMs = t0 + (i + 1) * 40 - Date.now();
  if (waitMs > 2) await sleep(waitMs);
}
await page.mouse.up();
console.log(`[session] released after streaming; waiting for outcome...`);

// wait for the outcome line, then check the pipeline drained
let outcomeLine = -1;
for (let i = 0; i < 60; i++) {
  await sleep(500);
  const lines = fs.readFileSync(LOG, "utf8").split("\n");
  for (let n = beforeLines; n < lines.length; n++) {
    if (lines[n].includes("dictation outcome")) outcomeLine = n;
  }
  if (outcomeLine >= 0) break;
}
if (outcomeLine < 0) {
  console.log("[result] NO OUTCOME LINE in 30s - different failure shape; not sampling");
} else {
  console.log(`[result] outcome logged; waiting 8s for the pipeline tail...`);
  await sleep(8000);
  const lines = fs.readFileSync(LOG, "utf8").split("\n");
  const tail = lines.slice(outcomeLine).join("\n");
  const drained = /Unregistered handy-keys shortcut: cancel/.test(tail);
  const trayIdle = /busy=false/.test(tail);
  console.log(`[result] guard-dropped=${drained} tray-idle=${trayIdle} transcript-frame=${gotTranscript}`);
  if (!drained) {
    console.log(`[result] WEDGE REPRODUCED - sampling pid ${pid}`);
    execSync(`sample ${pid} 5 -file ${OUT}/wedged-v166-sample.txt`);
    console.log(`[result] sample written to ${OUT}/wedged-v166-sample.txt`);
  }
}
await browser.close();
