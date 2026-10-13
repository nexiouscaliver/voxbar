// Probe: can a NEW connection still pair (register_connection locks the
// companion `connections` mutex)? Decides whether the post-save hang left
// that mutex held (deadlock) or killed only the one task.
import { chromium } from "/Users/shahil/work/regenai-repo/mission-control/voxbar-merge/node_modules/playwright/index.mjs";
import fs from "node:fs";

const APP_SUPPORT = "/Users/shahil/Library/Application Support/com.voxbar.app";
const token = JSON.parse(fs.readFileSync(`${APP_SUPPORT}/settings_store.json`, "utf8")).settings
  .companion_pairing_token;
const BASE = "https://100.114.70.58:4177";

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
    if (typeof f.payload === "string") frames.push(f.payload.slice(0, 200));
  });
});
await page.goto(`${BASE}/#t=${token}`, { waitUntil: "domcontentloaded" });
try {
  await page.waitForSelector("#state.ready", { timeout: 10000 });
  console.log("PROBE: new connection PAIRED (welcome received; connections lock is free)");
} catch (e) {
  console.log("PROBE: new connection NEVER paired (connections lock likely held) — state:", 
    await page.evaluate(() => document.getElementById("stateText").textContent).catch(() => "?"));
}
console.log("PROBE frames:", JSON.stringify(frames));
await browser.close();
