// Cycle 17 wedge catch: run phone-lane cycles against the LIVE symbol-rich
// v1.6.8 build until the self-deadlock forms, then freeze the evidence.
// Trigger model from the hang reports: phone hello -> within ~12-30s a
// tokio worker self-deadlocks on a mutex (guard-across-await class); main
// blocks when a session teardown next touches that mutex.
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";
import { execSync } from "node:child_process";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const LOG = "/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log";
const OUT = "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/shots/proof-v1.6.9";
const CYCLES = parseInt(process.env.CYCLES || "6", 10);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const logText = fs.readFileSync(LOG, "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;
console.log(`[setup] base=${BASE} token=${token.slice(0, 6)}...`);

const dir = `${APP_SUPPORT}/recordings`;
const WAV = fs
  .readdirSync(dir)
  .filter((f) => f.endsWith(".wav"))
  .map((f) => `${dir}/${f}`)
  .sort()
  .slice(-1)[0];
if (!WAV) throw new Error("no recordings to stream");

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
console.log(`[setup] wav=${WAV.split("/").pop()} (${(chunks.length * 640 / 16000).toFixed(2)}s)`);

// A thread parked on a mutex for the whole 2s sample = stuck. Normal brief
// contention never survives the entire window with identical frames.
function stuckMutexThreads(pid) {
  try {
    execSync(`sample ${pid} 2 -file /tmp/voxbar-wedge3/probe.txt 2>/dev/null`);
  } catch {
    return [];
  }
  const t = fs.readFileSync("/tmp/voxbar-wedge3/probe.txt", "utf8");
  const out = [];
  const blocks = t.split("\n    ").filter((b) => b.includes("__psynch_mutexwait"));
  for (const b of blocks) {
    const head = b.split("\n")[0];
    const total = parseInt((head.match(/(\d+)\s+Thread_/) || [0, 0])[1], 10);
    const mut = parseInt((b.match(/(\d+)\s+__psynch_mutexwait/) || [0, 0])[1], 10);
    if (total > 0 && mut / total > 0.9) out.push(head.trim().slice(0, 120));
  }
  return out;
}

const browser = await chromium.launch({ headless: false });
const context = await browser.newContext({
  ignoreHTTPSErrors: true,
  viewport: { width: 390, height: 844 },
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

let wedgeCaught = false;
for (let cycle = 1; cycle <= CYCLES && !wedgeCaught; cycle++) {
  if (process.env.RELAUNCH === "1") {
    execSync(`osascript -e 'tell application "VoxBar" to quit'`, { stdio: "ignore" });
    await sleep(3000);
    execSync("open /Applications/VoxBar.app");
    console.log(`[cycle ${cycle}] relaunched; waiting 20s (tonight's phone-connect timing)...`);
    await sleep(20000);
  }
  const pid = parseInt(execSync("pgrep -x voxbar | head -1").toString().trim(), 10);
  console.log(`\n[cycle ${cycle}] pid=${pid}`);

  // Fresh page = phone reconnect (hello -> recents -> tray resync)
  const p = await context.newPage();
  await p.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
  await p.waitForSelector("#state.ready", { timeout: 20000 });
  console.log(`[cycle ${cycle}] connected; idling past the post-hello window...`);
  await sleep(15000);

  let stuck = stuckMutexThreads(pid);
  if (stuck.length) {
    console.log(`[cycle ${cycle}] STUCK BEFORE SESSION: ${JSON.stringify(stuck)}`);
    wedgeCaught = true;
    break;
  }

  await p.click("#tgtPhone");
  const b = await p.locator("#ptt").boundingBox();
  await p.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
  await p.mouse.down();
  const t0 = Date.now();
  for (let i = 0; i < chunks.length; i++) {
    await p.evaluate((c) => sendChunk(c), chunks[i]);
    const waitMs = t0 + (i + 1) * 40 - Date.now();
    if (waitMs > 2) await sleep(waitMs);
  }
  await p.mouse.up();
  console.log(`[cycle ${cycle}] released; waiting 20s for outcome+drain...`);
  await sleep(20000);

  const lines = fs.readFileSync(LOG, "utf8").split("\n");
  const tail = lines.slice(-400).join("\n");
  const drained = /Unregistered handy-keys shortcut: cancel/.test(tail);
  console.log(`[cycle ${cycle}] recent-drain-line=${drained}`);

  stuck = stuckMutexThreads(pid);
  if (stuck.length) {
    console.log(`[cycle ${cycle}] STUCK AFTER SESSION: ${JSON.stringify(stuck)}`);
    wedgeCaught = true;
    break;
  }
  console.log(`[cycle ${cycle}] clean`);

  // reconnect pattern: reload the page mid-cycle (the operator's phone
  // reconnected twice 35s apart before tonight's wedge)
  await p.close();
}

if (wedgeCaught) {
  const pid = parseInt(execSync("pgrep -x voxbar | head -1").toString().trim(), 10);
  execSync(`sample ${pid} 10 -file ${OUT}/wedged-symbol-build.txt`);
  console.log(`[caught] FULL SAMPLE -> ${OUT}/wedged-symbol-build.txt`);
  // give macOS a chance to write its own hang report too
  await sleep(70000);
}
await browser.close();
console.log(wedgeCaught ? "[done] WEDGE CAUGHT" : "[done] no wedge in cycles");
