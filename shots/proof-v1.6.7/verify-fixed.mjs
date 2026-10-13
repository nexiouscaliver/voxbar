// v1.6.7 verification: the exact wedge shape (hold-mode phone-target
// session) driven TWICE against the fixed installed app, then a THIRD
// session proving the app still accepts new recordings. The wedge used to
// reproduce on 100 percent of these.
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";
import { execSync } from "node:child_process";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const LOG = "/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logText = fs.readFileSync(LOG, "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;
console.log(`[setup] base=${BASE}`);

// pick the two newest WAVs (each session's history cleanup deletes the old)
function newestWavs(n) {
  const dir = `${APP_SUPPORT}/recordings`;
  return fs.readdirSync(dir).filter((f) => f.endsWith(".wav")).map((f) => `${dir}/${f}`).sort().slice(-n);
}

function loadWavChunks(path, maxSecs) {
  const buf = fs.readFileSync(path);
  let off = 12, dataOff = -1;
  while (off + 8 <= buf.length) {
    const id = buf.toString("ascii", off, off + 4);
    const size = buf.readUInt32LE(off + 4);
    if (id === "data") { dataOff = off + 8; break; }
    off += 8 + size + (size % 2);
  }
  const n = Math.min(Math.floor((buf.length - dataOff) / 2), maxSecs * 16000);
  const chunks = [];
  for (let i = 0; i < n; i += 640) {
    const len = Math.min(640, n - i);
    const c = new Array(len);
    for (let j = 0; j < len; j++) c[j] = buf.readInt16LE(dataOff + (i + j) * 2) / 32768;
    chunks.push(c);
  }
  return chunks;
}

const browser = await chromium.launch({ headless: false, args: ["--autoplay-policy=no-user-gesture-required"] });

async function runSession(label, wav) {
  const beforeLines = fs.readFileSync(LOG, "utf8").split("\n").length;
  const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } });
  await context.addInitScript(() => {
    navigator.mediaDevices.getUserMedia = async () => {
      const ac = new AudioContext();
      return ac.createMediaStreamDestination().stream;
    };
  });
  const page = await context.newPage();
  await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#state.ready", { timeout: 20000 });
  await page.click("#tgtPhone");
  const chunks = loadWavChunks(wav, 4);
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
  // wait for outcome or zero-audio completion
  let verdict = { outcome: false, guard: false, trayIdle: false, wedge: true };
  for (let i = 0; i < 50; i++) {
    await sleep(500);
    const lines = fs.readFileSync(LOG, "utf8").split("\n");
    const tail = lines.slice(beforeLines).join("\n");
    const outcome = /dictation outcome/.test(tail) || /produced no audio samples/.test(tail);
    const guard = /Unregistered handy-keys shortcut: cancel/.test(tail);
    const trayIdle = /busy=false/.test(tail);
    if (outcome && guard && trayIdle) { verdict = { outcome: true, guard: true, trayIdle: true, wedge: false }; break; }
    if (outcome && guard) verdict = { outcome: true, guard: true, trayIdle, wedge: false };
    else if (outcome) verdict = { outcome: true, guard, trayIdle, wedge: !trayIdle };
  }
  console.log(`[${label}] outcome=${verdict.outcome} guard-dropped=${verdict.guard} tray-idle=${verdict.trayIdle} WEDGE=${verdict.wedge}`);
  await context.close();
  return verdict;
}

let wavs = newestWavs(3);
const results = [];
for (let s = 1; s <= 3; s++) {
  const wav = wavs[(s - 1) % wavs.length];
  results.push(await runSession(`session-${s}`, wav));
  await sleep(1500);
}
const wedged = results.filter((r) => r.wedge).length;
console.log(`[RESULT] ${3 - wedged}/3 sessions completed cleanly; wedge count: ${wedged}`);
await browser.close();
process.exit(wedged === 0 ? 0 : 1);
