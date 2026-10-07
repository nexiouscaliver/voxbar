# VoxBar v1 — feature synthesis (analyst verdict)

Input: the operator's five-goal list, checked against the four research notes
(`docs/vox/research-build.md`, `research-models.md`, `research-ui.md`,
`research-engines.md`) **and re-verified against the code they cite** in this
session (upstream `cjpais/Handy` at `c8262ab`, confirmed via
`git -C handy-dictation log --oneline -2`). Every code claim below was read or
run during this session unless marked otherwise. All paths relative to
`handy-dictation/`.

Method note: for each goal I asked "does upstream already ship this?" and went
to the cited code. Checks run this session: reads/greps of `tauri.conf.json`,
`tray.rs`, `lib.rs`, `memory.rs`, `settings.rs`, `model.rs`,
`transcription.rs`, `Cargo.toml`, `catalog.json` (via `python3` JSON parse),
`package.json`, `nsis/installer.nsi`, `portable.rs`, locale greps, icon dir
listings, and one web search (parakeet-mlx context, sources at the end). No
builds or test suites were run — this is an analysis task; nothing here
modifies source.

---

## Feature table

| # | id | Title | Upstream? | Size | v1 | One-line verdict |
|---|---|---|---|---|---|---|
| 1 | `voxbar-rebrand` | Rebrand fork as VoxBar: identity, titles, neutral brand assets | no | M | ✅ | Nothing exists upstream *to* keep — this is the legally required core of the fork |
| 2 | `tray-model-ram-status` | Tray: resident model + approximate RAM footprint | no (unload action IS upstream) | M | ✅ | Unload-now already ships; the RAM footprint and resident-vs-selected display are genuinely new |
| 3 | `memory-pressure-gate` | Pre-load free-RAM check: refuse with clear warning (+ settings toggle) | no | M | ✅ | No memory probing exists anywhere in the crate; single load choke point makes the gate clean |
| 4 | `defaults-24gb` | 24 GB-sensible defaults: shorter unload-timeout default + recommended preset | no (preset *mechanism* is upstream) | S | ✅ | Timeout default is one line (`Min5` → shorter); preset choice rides existing rank machinery |
| 5 | `apple-silicon-verdict` | Highest-accuracy Apple Silicon setup = upstream Metal GGUF; no mlx sidecar | **yes** | S | ❌ (docs only) | transcribe-cpp static-Metal + catalog GGUF (acc 91–92) already wins; sidecar is not clean |

Dropped because upstream covers it (proof paths in the section at the end):
the tray Unload-model action, the recommended-model *mechanism*, and the
parakeet-mlx sidecar.

---

## Per-feature notes

### 1. `voxbar-rebrand` — Rebrand as VoxBar (M, in v1)

**alreadyUpstream: false.** Upstream *is* the Handy brand; every identity
surface must change. Verified surfaces this session:

- `src-tauri/tauri.conf.json:4-5` — `productName: "Handy"`,
  `identifier: "com.pais.handy"`; `:50,62` map `transcribe-libs` →
  `/usr/lib/Handy` (deb + rpm); `bundle.icon` list at the same file points at
  the Handy icon set; `plugins.updater` endpoint points at
  `https://github.com/cjpais/Handy/releases/latest/download/latest.json`
  (read via python3 JSON parse this session).
- `src-tauri/Cargo.toml:2-5` — package `name = "handy"`, `description =
  "Handy"`, `authors = ["cjpais"]`. `package.json:2` — `"name": "handy-app"`.
- Window title: `src-tauri/src/lib.rs:948` — `.title("Handy")` on the main
  `WebviewWindowBuilder` (`tauri.conf.json` `app.windows` is `[]`; the window
  is created in code).
- Tray title: `src-tauri/src/tray.rs:446-452` — `version_label()` renders
  `"Handy v{}"` / `"Handy v{} (Dev)"`; also the tooltip (`tray_tooltip()`,
  `tray.rs:442-444`).
- HTML title: `index.html:6` — `<title>handy</title>` (overlay title
  "Recording Overlay" is descriptive, keep).
- Linux rpath: `src-tauri/build.rs:19` — links
  `-Wl,-rpath,$ORIGIN/../lib/Handy:$ORIGIN/../lib` (must match whatever the
  deb/rpm `files` mapping becomes).
- Brand assets (must be *replaced*, not edited): `src-tauri/icons/` — `icon.icns`,
  `icon.ico`, `32x32/64x64/128x128(.@2x).png`, `logo.png`, `icon.png`, 11
  Windows-Store `Square*Logo/StoreLogo.png`, plus `android/` and `ios/` sets
  (dir listing this session). Runtime tray art: `src-tauri/resources/handy.png`
  (Linux "Colored" theme idle icon — `tray.rs:191-215` selects it),
  `handy_warning.png`, and the `tray_{idle,recording,transcribing}*.png`
  family which carry the Handy glyph. In-app logo components:
  `src/components/icons/HandyHand.tsx`, `HandyTextLogo.tsx` (rendered in
  About/onboarding — grep this session).
- Strings: 41 "Handy" matches across `src/**/*.{ts,tsx}` (incl.
  `AboutSettings.tsx:74` linking `https://github.com/cjpais/Handy`), 17 in
  `src/i18n/locales/en/translation.json`, mirrored in all 26 locale files
  (grep counts this session). Note `CONTRIBUTING_TRANSLATIONS.md` says brand
  names are deliberately untranslated — the key set itself is parity-gated by
  `bun run check:translations` in CI (`code-quality.yml:50`), so rename/replace
  values, don't add keys.
- NSIS: `src-tauri/nsis/installer.nsi` — Handy references incl. the on-disk
  portable marker `"Handy Portable Mode"` (`installer.nsi:600`). **Trap**:
  that string is a wire-format marker written by old releases; keep marker
  compatibility (or accept a clean break from old Handy portable installs —
  decide explicitly).
- **Updater**: `createUpdaterArtifacts: true` + upstream endpoint + upstream
  minisign pubkey. The fork cannot sign updates to cjpais's release feed; v1
  must either empty the endpoints / disable updater (the
  `HANDY_DISABLE_UPDATER=1` path already exists — `settings.rs:1231-1238`,
  used by Nix, `flake.nix:89-186`) or repoint to a VoxBar feed later.
- **License**: MIT (`LICENSE`, "Copyright (c) 2025 CJ Pais") covers code;
  rebrand must *retain* the copyright notice and attribution (e.g. an
  "originally Handy by CJ Pais" line in About) while replacing brand assets —
  that is consistent with the operator's "MIT covers code only" framing.

Size M: individually mechanical, but the surface is wide (3 version-sync
files, 2 build systems' paths, generated asset sets for 4 platforms, 26
locales, an NSIS wire-marker decision, updater strategy). Files to touch:
`src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`, `package.json`,
`src-tauri/build.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/tray.rs`,
`index.html`, `src-tauri/icons/**` (regenerate neutral set),
`src-tauri/resources/{handy,handy_warning,tray_*}.png`,
`src/components/icons/HandyHand.tsx` + `HandyTextLogo.tsx`,
`src/components/settings/about/AboutSettings.tsx`,
`src/i18n/locales/*/translation.json` (26), `src-tauri/nsis/installer.nsi`,
plus `src-tauri/tauri.windows.conf.json` if it repeats resource paths.

### 2. `tray-model-ram-status` — Tray resident model + RAM footprint (M, in v1)

**alreadyUpstream: false as a feature; its "Unload now" sub-part IS upstream**
(moved to droppedBecauseUpstream). What I verified:

- The tray already shows the *selected* model as the model-submenu label
  (`tray.rs:533-547`: `find(|(id,_)| *id == inputs.selected_model)`) and
  carries only a bool `model_loaded` in `MenuInputs` (`tray.rs:54-66`,
  populated from `is_model_loaded()` in `compute_desired`, `tray.rs:317`).
  Selected ≠ resident in `Immediately` unload mode and under headless
  `--model` overrides (`research-models.md` §5.1) — minor correctness delta.
- **Nothing measures RAM**: grep of `src-tauri/src` for
  `sysinfo|memory_usage|resident_set|task_vm|proc_pid_info|GlobalMemoryStatusEx|meminfo`
  returned zero code hits this session; `Cargo.toml` has no sysinfo crate
  (grep exit 1); `LoadedInfo` (`protocol.rs:144-156`) carries arch/backend/
  device/on_gpu/capabilities but **no memory field**.
- The worker's pid is already tracked (`Control { pid, ... }`,
  `supervisor.rs:1063`), so parent-side RSS polling for transcribe-cpp models
  is possible without protocol changes; ONNX models live in the *app* process
  (`transcription.rs:186-194`) and need an in-process measurement or an
  explicitly-labeled estimate.

v1 scope (conservative): add a memory module probe + one disabled tray line
of the form `parakeet-tdt-0.6b-v3 — ~460 MB` where the number is (a) worker
RSS from the tracked pid for TranscribeCpp models, (b) a `size_mb`-derived
estimate labeled "~" for in-process ONNX models; refresh it wherever
`update_tray_menu` already fires (`model-state-changed` listener,
`lib.rs:355-359`). Files: `src-tauri/src/memory.rs` (grow into probe module),
`src-tauri/src/tray.rs` (`MenuInputs` + `build_menu` + tooltip),
`src-tauri/src/lib.rs` (snapshot plumbing), accessor on
`src-tauri/src/engine_supervisor/supervisor.rs` (pid at `:1063` is private),
i18n `tray.*` keys (must land in all 26 locales — `check:translations` gates
key parity; `build.rs:284-361` regenerates the Rust struct automatically).
Note the diffing applier only rebuilds when `MenuInputs` changes
(`PartialEq`, `tray.rs:57-58`) — a RAM number that updates needs its own
change signal or it will be stale until the next model event; simplest v1
updates it only on model load/unload, which is when it matters.

### 3. `memory-pressure-gate` — Pre-load free-RAM check (M, in v1)

**alreadyUpstream: false.** Verified: the single load choke point
`load_model_with_device` (`transcription.rs:527`) goes straight from
`apply_accelerator_settings` (`:532`) to the old-engine drop (`:590-602`) with
no memory consideration; `memory.rs` is glibc allocator tuning only (Linux
`M_MMAP_THRESHOLD` + `malloc_trim`, no-ops on macOS — read in full this
session); no sysinfo/memory-probe code anywhere (grep above).

v1 scope (conservative, satisfies "never silently head into an OOM"):
- `memory.rs`: add a free-RAM probe (macOS `sysctl hw.memsize` +
  `host_statistics64` vm stats; Linux `/proc/meminfo`; Windows
  `GlobalMemoryStatusEx`) — macOS-first, others follow.
- Gate in `load_model_with_device` **before the old-engine drop at `:590`**
  (so the outgoing model's footprint can be credited — peak = max(old,new),
  `research-models.md` §8): forecast = `ModelInfo.size_mb` (disk-size proxy,
  the only size signal in-app — `model.rs:67,240`; label it an estimate, the
  note explicitly warns GGUF disk ≠ resident RAM). If forecast + headroom >
  free RAM → refuse: emit `loading_failed` with a clear "not enough free
  memory (need ~X, have Y)" error (the event path already exists,
  `transcription.rs:538-546`; frontend toasts on `loading_failed`,
  `App.tsx:197-215` per `research-models.md` §5.2).
- Settings toggle (`memory_pressure_guard: bool`, default on) following the
  canonical 7-touch adding-a-setting recipe in `research-ui.md` §2.6:
  `settings.rs` field+default → `change_*` command (`shortcut/mod.rs`
  pattern) → `collect_commands!` (`lib.rs:652-771`) → debug build to regen
  `src/bindings.ts` (`lib.rs:778-784`) → `settingUpdaters`
  (`settingsStore.ts:83-200`) → component + i18n keys.
- Auto-select-smaller-fallback: **stretch, not v1-blocking** — the operator's
  requirement is "either … or"; refuse-with-warning satisfies it. Fallback
  selection would reuse `get_available_models` rank order
  (`model.rs:1184-1202`) filtered by size.

Files: `src-tauri/src/memory.rs`, `src-tauri/src/managers/transcription.rs`,
`src-tauri/src/settings.rs`, `src-tauri/src/shortcut/mod.rs`,
`src-tauri/src/lib.rs`, `src/stores/settingsStore.ts`,
`src/components/settings/**` (new toggle),
`src/i18n/locales/*/translation.json`, `src/bindings.ts` (regenerated).

### 4. `defaults-24gb` — 24 GB-sensible defaults (S, in v1)

**alreadyUpstream: false for the timeout default; the preset *mechanism* is
upstream.** Verified:

- Upstream default unload timeout is `Min5` (`settings.rs:141` `#[default]`
  on `Min5`; applied at `:958`). Changing to `Min2` is the whole code change;
  the frozen v0.9 fixture (`settings.rs:1383` parses `"min5"`) is unaffected
  because it exercises an *explicit* stored value, not the default. Mind the
  documented trap: generated TS bindings mislabel the wire strings
  (`"min_5"` vs real serde `"min5"`) — new frontend code must send
  `"min2"`-style values (`research-models.md` §8; `ModelUnloadTimeout.tsx:30-46`
  already does).
- Recommended preset: the *machinery* is upstream — catalog
  `recommended`/`recommended_rank` (rank 1 `parakeet-unified-en-0.6b-gguf`
  acc 90/streaming; rank 2 `nemotron-3.5-asr-streaming` 28 langs — extracted
  from `catalog.json` this session), `auto_select_model_if_needed` picks the
  first downloaded model in that rank order once onboarding completes
  (`model.rs:1544-1597`, read this session). For a 24 GB machine the rank-1
  default (~455 MB Q4_K_M) fits trivially. The only VoxBar work is the
  *decision* (confirm rank-1 streaming parakeet, or pin e.g. the higher-
  accuracy 1.1b variant at 787 MB which still fits easily) and at most a tiny
  onboarding/default tweak.

Files: `src-tauri/src/settings.rs` (default), optionally
`src/components/onboarding/Onboarding.tsx` (preset highlight). Size S.

### 5. `apple-silicon-verdict` — Analyst verdict (alreadyUpstream: yes; docs only)

**Verdict: upstream already provides the highest-accuracy local engine setup
for Apple Silicon; do not build a parakeet-mlx sidecar.** Evidence:

1. On macOS, transcribe-cpp 0.3.1 is compiled with the **static `metal`
   backend** (`Cargo.toml:157-159`, read this session) — every catalog GGUF
   model runs GPU-accelerated in an isolated worker process
   (`engine_supervisor/mod.rs:1-40`), with crash→CPU retry and GPU-quarantine
   recovery (`supervisor.rs:724-756`).
2. Catalog accuracy (parsed from `catalog.json` this session, 69 models):
   `cohere-transcribe-03-2026` **92** (rank 4, recommended, 14 langs),
   `granite-speech-4.1-2b` **92**, `parakeet-tdt-1.1b`/`parakeet-rnnt-1.1b`
   **91** (en), `parakeet-rnnt-0.6b` 90, vs `whisper-large-v3` 89. All
   one-click downloadable, sha-pinned, mirror-backed.
3. Upstream's built-in **Parakeet ONNX** support exists
   (`ParakeetModel::load(..., Int8)`, `transcription.rs:677`) but is
   **CPU-only on macOS** — transcribe-rs is built with just the `onnx`
   feature (`Cargo.toml:83`), and no CoreML execution-provider feature exists
   anywhere in the manifest (grep `coreml` = no match). Its legacy-table
   accuracy (v2 0.85 / v3 0.80) is also below the GGUF parakeets on the same
   editorial scale. So "Parakeet via ONNX" is *not* the high-accuracy path —
   "Parakeet via GGUF on Metal" is, and it's already shipped.
4. A parakeet-mlx sidecar fails the operator's "only if clean" bar: it would
   be a third engine home (mirror the EngineSupervisor worker+framed-protocol
   pattern or add an in-process slot — `research-engines.md` §5.3), needs its
   own model distribution (catalog is hardcoded TranscribeCpp,
   `catalog/mod.rs:93`), gets no streaming/live-overlay integration for free
   (`transcription.rs:883-895`), and duplicates accuracy the Metal path
   already delivers. External context (web search this session): parakeet-mlx
   is real and competitive on Apple Silicon, but it is a separate runtime —
   nothing found contradicts "integration cost > benefit given parity".

Action: record the verdict (this note), configure the preset (feature 4) to a
high-accuracy catalog GGUF, revisit a sidecar only if a concrete gap appears
(e.g. mlx-exclusive model or streaming need). No v1 code.

---

## Dropped because upstream already covers it

1. **Tray "Unload model now" action** — proof: `src-tauri/src/tray.rs:549-556`
   (menu item `unload_model`, enabled iff `inputs.model_loaded`) + handler
   `src-tauri/src/lib.rs:303-311` (`request_unload()` with a no-model warn) +
   the existing frontend command `unload_model_manually`
   (`src-tauri/src/commands/transcription.rs:33-40`). Verified by direct read
   this session. Feature 2 keeps only the *status display* delta.
2. **Recommended-model preset mechanism** — proof:
   `src-tauri/src/managers/model.rs:1544-1597`
   (`auto_select_model_if_needed`, ranked pick) + `catalog.json`
   `recommended`/`recommended_rank` fields (rank 1/2 extracted this session) +
   onboarding presenting the choice (`model.rs:1566-1572`). Only the timeout
   default and the *choice* of preset remain (feature 4).
3. **parakeet-mlx sidecar for highest-accuracy Apple Silicon ASR** — proof:
   `src-tauri/Cargo.toml:157-159` (static Metal) + catalog entries at acc
   91–92 (`src-tauri/src/catalog/catalog.json`, parsed this session) +
   `src-tauri/src/managers/transcription.rs:677` (ONNX parakeet exists but is
   CPU-only; no CoreML feature in `Cargo.toml`). Upstream's Metal GGUF path
   is already the best local accuracy available to the fork.

---

## Recommended task ordering

1. **`voxbar-rebrand`** first — everything else lands on a fork that is
   legally clean to distribute; it also touches `tauri.conf.json`/`tray.rs`/
   locales that features 2–4 touch, so doing it first avoids rebase churn.
   Decide the two traps early: NSIS portable marker compat and updater
   disable-vs-repoint.
2. **`defaults-24gb`** — one-line default + preset decision; instant value,
   no dependencies (also picks the model whose RAM footprint feature 2 will
   display).
3. **`memory-pressure-gate`** — before the tray work: it creates the
   `memory.rs` probe module that feature 2 reuses, and it protects the
   operator's 24 GB machine (their stated OOM history) from day one.
4. **`tray-model-ram-status`** — builds on the probe module; display-only
   since unload already exists; lands last because it is the most cosmetic.
5. **`apple-silicon-verdict`** — no code; it *is* this document + the feature-4
   preset decision. Revisit only on a concrete gap.

Cross-cutting reminders for whoever builds: new Rust commands need a debug
build to regenerate `src/bindings.ts` (`lib.rs:778-784`); new settings follow
the 7-step recipe (`research-ui.md` §2.6); new tray strings must land in all
26 locale files in the same change (`check:translations` CI gate); Rust tests
enter CI only via `cargo test` in `test.yml` (model-dependent suites are
`#[ignore]`d, `supervisor.rs:1477-1502`); version bumps touch package.json +
Cargo.toml + tauri.conf.json together (`research-build.md` §2).

---

### Sources (web context for the mlx verdict, searched this session)

- [Parakeet vs Whisper: which speech model for Mac dictation? — Suprflow.app](https://suprflow.app)
- [Qwen3-ASR vs Parakeet vs Whisper: WER and speed — Soniqo.audio](https://soniqo.audio)
- [whisper.cpp vs whisper-diarization — LibHunt](https://www.libhunt.com)
