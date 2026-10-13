// Pairing probe against the WEDGED app: can a NEW phone connection still
// pair (register_connection takes the companion connections mutex)?
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;
const logText = fs.readFileSync("/Users/shahil/Library/Logs/com.voxbar.app/voxbar.log", "utf8");
const urlMatch = [...logText.matchAll(/companion: server running on (https:\/\/\S+)/g)];
const BASE = urlMatch[urlMatch.length - 1][1];

const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({ ignoreHTTPSErrors: true });
await context.addInitScript(() => {
  navigator.mediaDevices.getUserMedia = async () => {
    const ac = new AudioContext();
    return ac.createMediaStreamDestination().stream;
  };
});
const page = await context.newPage();
let frames = [];
page.on("websocket", (ws) => {
  ws.on("framereceived", (f) => {
    if (typeof f.payload === "string") frames.push(f.payload.slice(0, 120));
  });
});
await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
try {
  await page.waitForSelector("#state.ready", { timeout: 12000 });
  console.log("PAIR-PROBE: PAIRED (welcome received; connections lock is FREE)");
} catch (e) {
  console.log(
    "PAIR-PROBE: NEVER PAIRED in 12s (connections lock likely HELD by a dead task)",
  );
}
console.log("PAIR-PROBE frames:", JSON.stringify(frames));
await browser.close();
