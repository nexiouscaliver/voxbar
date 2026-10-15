// v1.6.9 verification: the KB-300 fix's success metric is the log line
// that appeared in exactly 1 of 11 historical phone sessions - the
// post-save recents broadcast RETURNING ("post-save recent-history
// refresh finished"). Every session with a connected phone must now log
// it, the app must stay responsive, and quitting with a lane live must
// actually exit (the KB-301 fuse).
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";
import { execSync } from "node:child_process";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const LOG = "/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log";
const SESSIONS = parseInt(process.env.SESSIONS || "4", 10);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logText = fs.readFileSync(LOG, "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;

const dir = `${APP_SUPPORT}/recordings`;
const WAV = fs.readdirSync(dir).filter((f) => f.endsWith(".wav")).map((f) => `${dir}/${f}`).sort().slice(-1)[0];
const buf = fs.readFileSync(WAV);
let off = 12, dataOff = -1, dataLen = 0;
while (off + 8 <= buf.length) {
  const id = buf.toString("ascii", off, off + 4);
  const size = buf.readUInt32LE(off + 4);
  if (id === "data") { dataOff = off + 8; dataLen = size; break; }
  off += 8 + size + (size % 2);
}
const n = Math.min(Math.floor(dataLen / 2), 3 * 16000);
const chunks = [];
for (let i = 0; i < n; i += 640) {
  const len = Math.min(640, n - i);
  const c = new Array(len);
  for (let j = 0; j < len; j++) c[j] = buf.readInt16LE(dataOff + (i + j) * 2) / 32768;
  chunks.push(c);
}

const browser = await chromium.launch({ headless: false });
const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } });
await context.addInitScript(() => {
  try {
    navigator.mediaDevices.getUserMedia = async () => {
      const ac = new (window.AudioContext || window.webkitAudioContext)();
      return ac.createMediaStreamDestination().stream;
    };
  } catch (e) {}
});
const page = await context.newPage();
await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
await page.waitForSelector("#state.ready", { timeout: 20000 });
console.log("[connect] lane up");

let refreshReturns = 0;
let outcomes = 0;
for (let s = 1; s <= SESSIONS; s++) {
  const before = fs.readFileSync(LOG, "utf8").split("\n").length;
  await page.click("#tgtPhone");
  const b = await page.locator("#ptt").boundingBox();
  await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await page.mouse.down();
  const t0 = Date.now();
  for (let i = 0; i < chunks.length; i++) {
    await page.evaluate((c) => sendChunk(c), chunks[i]);
    const w = t0 + (i + 1) * 40 - Date.now();
    if (w > 2) await sleep(w);
  }
  await page.mouse.up();
  // wait for outcome + the broadcast return
  let sawOutcome = false, sawRefresh = false;
  for (let i = 0; i < 50; i++) {
    await sleep(400);
    const lines = fs.readFileSync(LOG, "utf8").split("\n");
    const tail = lines.slice(before).join("\n");
    sawOutcome = tail.includes("dictation outcome");
    sawRefresh = tail.includes("post-save recent-history refresh finished");
    if (sawOutcome && sawRefresh) break;
  }
  outcomes += sawOutcome ? 1 : 0;
  refreshReturns += sawRefresh ? 1 : 0;
  console.log(`[session ${s}] outcome=${sawOutcome} recents-refresh-returned=${sawRefresh}`);
}

// reconnect cycle: hello must succeed on the still-healthy registry
await page.reload({ waitUntil: "domcontentloaded" });
await page.waitForSelector("#state.ready", { timeout: 20000 });
console.log("[reconnect] register after sessions OK");

// quit-with-lane-live: the app must actually exit (fuse at worst)
await browser.close();
execSync(`osascript -e 'tell application "VoxBar" to quit'`, { stdio: "ignore" });
let exited = false;
let quitSecs = 0;
for (let i = 0; i < 20; i++) {
  await sleep(500);
  try { execSync("pgrep -x voxbar", { stdio: "ignore" }); } catch { exited = true; quitSecs = (i + 1) * 0.5; break; }
}
console.log(`[quit] app exited within ${exited ? quitSecs : ">10"}s: ${exited}`);
const verdict = refreshReturns === SESSIONS && outcomes === SESSIONS && exited;
console.log(verdict ? "VERDICT: PASS" : `VERDICT: FAIL (${outcomes}/${SESSIONS} outcomes, ${refreshReturns}/${SESSIONS} refresh returns, exited=${exited})`);
process.exit(verdict ? 0 : 1);
