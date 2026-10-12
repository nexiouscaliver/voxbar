import assert from "node:assert/strict";
import fs from "node:fs";
import nodePath from "node:path";
import { fileURLToPath } from "node:url";

// v1.6.3 pin for the phone client's capture resampler (the companion
// zero-audio defect). The v1.6.2 resampler loop left a NEGATIVE residual
// at every render-quantum boundary (pos == input.length - 1, then pos -=
// input.length => -1), read input[-1] = undefined on the next quantum, and
// interpolated it into a NaN once per 128 output samples at the 16 kHz
// context the page itself requests. Those NaNs reached Silero, whose
// `prob > threshold` is false for NaN, so the VAD read the entire stream
// unvoiced and withheld the whole recording - sample_count 0 and the
// companion_zero_audio notice in BOTH hold and tap-to-lock modes, while
// the phone's stride-8 level meter kept showing a lively bar.
//
// This script extracts the two SHIPPED resampler cores from the served
// HTML (AudioWorklet blob + ScriptProcessor fallback, delimited by
// RESAMPLER-CORE-*-START/END markers) and drives them at the context
// rates the page itself produces. Old core verified failing: NaNs at
// stream indices 127,255,383,511,639..., poisoning every 480-sample
// Silero frame; the fixed cores must emit only finite samples, keep the
// 640-sample chunk cadence, and interpolate a ramp exactly across block
// boundaries.

const __dirname = nodePath.dirname(fileURLToPath(import.meta.url));
const clientPath = nodePath.join(
  __dirname,
  "..",
  "..",
  "src-tauri",
  "src",
  "companion",
  "client",
  "index.html",
);

const html = fs.readFileSync(clientPath, "utf8");
const TARGET_RATE = 16000;

function extractCore(name: string): string {
  const start = html.indexOf(`// RESAMPLER-CORE-${name}-START`);
  const end = html.indexOf(`// RESAMPLER-CORE-${name}-END`);
  assert.ok(
    start >= 0 && end > start,
    `resampler core markers for ${name} not found in the client page`,
  );
  return html.slice(start, end);
}

function compileCore(coreSrc: string) {
  // The core contract at both call sites: reads `input`, `ratio`, `pos`,
  // `tail`; emits each output sample via emit(sample); returns the next
  // [pos, tail] state.
  return new Function(
    "input",
    "ratio",
    "pos",
    "tail",
    "emit",
    `"use strict";\n${coreSrc}\nreturn [pos, tail];`,
  ) as (
    input: Float32Array,
    ratio: number,
    pos: number,
    tail: number,
    emit: (s: number) => void,
  ) => [number, number];
}

// Feed render quanta of inputLen samples, collect the emitted 16 kHz
// stream (the 640-sample chunk cadence is preserved by the emit wrapper
// at each call site; here the raw stream order is what matters).
function drive(
  step: ReturnType<typeof compileCore>,
  inputLen: number,
  contextRate: number,
  totalInputs: number,
): number[] {
  const out: number[] = [];
  let pos = 0;
  let tail = 0;
  const ratio = contextRate / TARGET_RATE;
  const emit = (s: number) => out.push(s);
  for (let i = 0; i < totalInputs; i++) {
    const input = new Float32Array(inputLen);
    for (let j = 0; j < inputLen; j++) {
      input[j] =
        Math.sin((2 * Math.PI * 440 * (i * inputLen + j)) / contextRate) * 0.5;
    }
    [pos, tail] = step(input, ratio, pos, tail, emit);
  }
  return out;
}

const cores: Array<[string, string]> = [
  ["worklet", extractCore("WORKLET")],
  ["scriptprocessor", extractCore("FALLBACK")],
];

for (const [name, src] of cores) {
  const step = compileCore(src);

  // 1. Finite output at the rates the page's own contexts produce: the
  //    16 kHz context it requests (ratio 1), the 48 kHz and 44.1 kHz
  //    engines may hand out instead (integral and fractional ratios).
  for (const [rate, inputLen] of [
    [16000, 128],
    [48000, 128],
    [44100, 4096],
  ] as const) {
    const out = drive(step, inputLen, rate, 200);
    assert.ok(out.length > 5000, `${name} @${rate}: audio was produced`);
    const bad = out.filter((v) => !Number.isFinite(v));
    assert.equal(
      bad.length,
      0,
      `${name} @${rate}: no non-finite sample may reach the wire (found ${bad.length})`,
    );
  }

  // 2. The exact v1.6.2 defect signature: the NaNs sat at stream indices
  //    127,255,383,511,639 (one per 128-sample render quantum).
  {
    const out = drive(step, 128, 16000, 50);
    for (const i of [127, 255, 383, 511, 639]) {
      assert.ok(
        Number.isFinite(out[i]),
        `${name}: stream index ${i} must be finite (the per-quantum NaN slot)`,
      );
    }
  }

  // 3. Interpolation accuracy across block boundaries: a linear ramp
  //    resamples exactly (output k == input position k * ratio), so the
  //    boundary fold neither skips a sample (the bare clamp would) nor
  //    drifts.
  for (const contextRate of [16000, 22050, 44100, 48000]) {
    const ratio = contextRate / TARGET_RATE;
    const buf: number[] = [];
    let pos = 0;
    let tail = 0;
    const emit = (s: number) => buf.push(s);
    for (let i = 0; i < 100; i++) {
      const input = new Float32Array(128);
      for (let j = 0; j < 128; j++) input[j] = i * 128 + j;
      [pos, tail] = step(input, ratio, pos, tail, emit);
    }
    for (let k = 0; k < buf.length; k++) {
      const expected = k * ratio;
      assert.ok(
        Math.abs(buf[k] - expected) <= 1e-3,
        `${name} @${contextRate}: sample ${k} got ${buf[k]}, expected ${expected}`,
      );
    }
  }

  // 4. Sanitize: even a poisoned INPUT sample (a hostile or broken
  //    capture path) is dropped to 0, never forwarded as NaN.
  {
    const buf: number[] = [];
    const emit = (s: number) => buf.push(s);
    const input = new Float32Array(128);
    for (let j = 0; j < 128; j++) input[j] = j === 60 ? Number.NaN : 0.1;
    compileCore(src)(input, 1, 0, 0, emit);
    const bad = buf.filter((v) => !Number.isFinite(v));
    assert.equal(bad.length, 0, `${name}: poisoned input must not emit NaN`);
  }
}

// Both call sites must carry the core (the v1.6.2 bug lived in BOTH the
// worklet and the fallback; fixing one leaves the other poisoned).
assert.ok(
  html.includes("this.port.postMessage(this.buf.slice(0, ${CHUNK})"),
  "the worklet keeps its 640-sample chunk cadence",
);
assert.ok(
  html.includes("sendChunk(buf.splice(0, CHUNK))"),
  "the fallback keeps its 640-sample chunk cadence",
);
// Meter honesty (same defect family): the meter may only read life from
// SENT audio - the gate is computed before the meter update.
const sendChunk = (() => {
  const at = html.indexOf("function sendChunk");
  const until = html.indexOf("function stopMic", at);
  assert.ok(at >= 0 && until > at, "sendChunk present in the client page");
  return html.slice(at, until);
})();
assert.ok(
  sendChunk.includes("ws && ws.readyState === WebSocket.OPEN && recording"),
  "the send gate is unchanged",
);
assert.ok(
  sendChunk.indexOf("const live =") < sendChunk.indexOf("lastLevelAt > 90") &&
    sendChunk.includes('miclvl.style.width = "0%"'),
  "the meter updates only for gate-passed chunks and decays to empty otherwise",
);

console.log(
  "companionResampler: both capture cores finite at 16/22.05/44.1/48 kHz, " +
    "boundary-exact, poison-sanitized; meter gated on sent audio",
);
