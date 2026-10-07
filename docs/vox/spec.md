# VoxBar v1 — Product Spec

A fork of upstream `cjpais/Handy` (MIT), rebranded for one operator on an
M4 Pro / 24 GB RAM frequently under memory pressure. Inputs:
`docs/vox/research.md`, the feature analysis, the codebase at
`handy-dictation/` (upstream `c8262ab`); every code claim was verified in
the authoring + two review sessions, nothing built or run. Correction to
the research notes: the rank-1 preset's default download is `Q8_0` ≈ 697 MB
(`default_quant`, selected at `src-tauri/src/managers/model.rs:142-148`),
not the 455 MB Q4_K_M. Paths relative to `handy-dictation/`.


## 1. Global constraints (bind every feature)

- **C1 Offline.** Never regressed; v1 adds zero network calls, removes one
  (the updater, F1); catalog downloads stay user-initiated.
- **C2 RAM discipline.** No feature adds resident overhead (no polling
  timers, no new background threads). The pressure gate (F3) defaults ON.
- **C3 i18n.** New user-visible strings land as keys in all 26 locale files
  (`src/i18n/locales/*/translation.json`) in the same change; `bun run
  check:translations` (gated at `.github/workflows/code-quality.yml:50`)
  must pass. "VoxBar" is untranslated per `CONTRIBUTING_TRANSLATIONS.md`.
- **C4 Tests.** Every new decision/derivation is a pure function with unit
  tests (`cargo test`, CI via `test.yml`). No new model-dependent tests.
- **C5 Brand.** Name VoxBar, neutral placeholder icon; no Handy brand asset
  in any shipped surface. MIT notice retained.
- **C6 Build discipline.** Changed commands need a debug build to regen
  `src/bindings.ts` (`lib.rs:776-784`); version bumps touch `package.json` +
  `Cargo.toml` + `tauri.conf.json` together.

Build order (research.md §ordering): F1 → F2 → F3 → F4; F5 is a record.

## 2. F1 — `voxbar-rebrand` (M)

Replace every user-visible Handy identity surface; change no runtime
behavior.

**User-visible behavior.** Window title (`lib.rs:948` `.title("Handy")`),
tray version label (`tray.rs:446-452`, `"Handy v{}"` → `"VoxBar v{}"`),
tooltip, `<title>` (`index.html:6`), installer UI, and About all read VoxBar.
All icons/tray art regenerate as neutral placeholders (all of
`src-tauri/icons/`; `src-tauri/resources/`
`{handy,handy_warning,tray_*}.png`); `HandyHand.tsx`/`HandyTextLogo.tsx` →
a neutral mark. Identity: `productName`
(`tauri.conf.json:3`) → "VoxBar", `identifier` (`:5`) → `"com.voxbar.app"`;
`Cargo.toml:2-5`; `package.json:2` `"handy-app"` → `"voxbar-app"`; Linux
rpath + deb/rpm mapping move off `/usr/lib/Handy` together (`build.rs:19-20`,
`tauri.conf.json:50,62`). CLI: `cli.rs:5` clap `about = "Handy - Speech to
Text"` is user-visible help text → "VoxBar — Speech to Text", and clap
`name = "handy"` → `"voxbar"` with `Cargo.toml:8` `default-run` following
the binary rename. Lib crate `handy_app_lib` (`Cargo.toml:19`) renames to
`voxbar_app_lib` — internal identifier, no wire compat, 6 textual use
sites (`main.rs:5/20/29/30/34` + `audio_toolkit/bin/cli.rs:4`; the latter
sits in a `[[bin]]` target commented out at `Cargo.toml:22-25` — not
compiled, so a compile error will NOT catch it; rename it anyway). About
keeps attribution: "VoxBar is a
fork of Handy by CJ Pais (MIT)"; `AboutSettings.tsx:74`'s source link may
point at the upstream repo. CI/packaging follows the rename in the same
change or the first post-rename CI run breaks: build.yml deb-files remap
(:398), Linux smoke run + assertions (:663-666, :675-707 `usr/bin/handy$`,
`/usr/lib/Handy/lib*`), macOS otool checks (:590/:593 — currently
`|| true`, pin to the new binary), `default` input (:22); nix-check.yml
eval/build attrs (:94/:128-130) with flake.nix `pname` (:90) and
`mainProgram` (:183); asset prefixes (release.yml:77,
build-test.yml:41, pr-test-build.yml:47); the Windows signing display
name `-d Handy` (tauri.conf.json:73) → "VoxBar". Kept as-is: the
`blob.handy.computer` ORT download URLs (build.yml:366/:381 — upstream's
CDN, not ours to rename) and `nix-check.yml:53`'s cache name.

**Locale strings (work item, not optional).** Exact-case "Handy"-valued
keys, verified per locale: 17 in 24 locales (incl. `en`, e.g.
`whatsNew.title`, `settings.about.version.description`), 18 in `nl` (adds
`sidebar.about`/`settings.about.title` = "Over Handy"), 16 in `uk`; every
**value** is rewritten to "VoxBar" — values only, never add/rename keys
(key parity is what `check:translations` gates; C3).

**Settings surface.** None new. Existing update-check surfaces (tray
"Check for updates" item, Settings toggle) stay **visible-and-inert** in
v1: with updater config removed the check fails before any HTTP, and the
failure is already swallowed (`UpdateChecker.tsx:108-109` catches,
`console.error` only — verified).

**Decisions (binding).**
- *Updater:* v1 ships disabled — remove `bundle.createUpdaterArtifacts`
  (`tauri.conf.json:28`) and the `plugins.updater` pubkey/endpoint (`:82-85`).
  `HANDY_DISABLE_UPDATER` (`settings.rs:1231-1238`) stays for packaging
  compat.
- *Keyboard implementation:* keep the serde wire value `"handy_keys"` (enum
  `settings.rs:217`; frozen fixture `:1414`), module
  (`shortcut/handy_keys.rs`), and component filenames
  (`HandyKeysShortcutInput.tsx`) — renaming breaks stored settings and the
  v0.9 fixture. Rename only the visible strings: the label
  "Handy Keys" → "VoxBar Keys" (`KeyboardImplementationSelector.tsx:11`)
  and the failure toast "Failed to initialize HandyKeys: {}. Reverted to
  Tauri." (`shortcut/mod.rs:563-566`, surfaces via
  `change_keyboard_implementation_setting`).
- *Portable marker (three sites, one decision):* the magic string lives in
  Rust too — read side `is_valid_portable_marker` (`portable.rs:118-122`,
  match at `:121`) and legacy-upgrade write (`portable.rs:34`), besides
  NSIS (`:600` read — `$2 == "Handy Portable Mode"` only today, so the
  template MUST gain the "VoxBar Portable Mode" comparison — and `:771`
  write). BOTH read sides (Rust and NSIS) accept BOTH magic strings; both
  write sites write the new one. Fresh VoxBar portable installs then get Data/ redirect + HF_HOME
  (`portable.rs:40-46`); old Handy installs keep working via the legacy
  branch.
- *Installed-upgrade data continuity:* F1 renames `productName` +
  `identifier`, so the installed data dir moves (portable installs are
  unaffected — `Data/` sits next to the exe; resolution at
  `portable.rs:83-87` falls back to Tauri's `app_data_dir()`, whose exact
  identifier-vs-productName derivation is not verifiable without running
  the app — not run; AC7 pins the behavior). On first run, when the new
  dir holds no settings store and a legacy Handy dir exists, migrate it:
  `settings_store.json` (`settings.rs:878`), `models/`, `history.db` AND
  `recordings/` together (`managers/history.rs:79-80` — the WAVs the db
  rows reference), logs. Runs as an early-`run()` hook before the store
  plugin initializes (plugin registered `lib.rs:886`; store opened lazily
  via StoreExt at `settings.rs:1039/:1250` — same pattern as
  `portable::init()`). Per-item and idempotent: skip items already
  present, prefer same-volume rename, never delete the source dir on
  partial failure (log + retry next launch). The hardcoded
  `%APPDATA%/handy*` display strings (`DebugPaths.tsx:29/37/46`) are
  replaced with backend-resolved paths so the Debug panel cannot go stale.
  *Autostart continuity:* the macOS agent plist is
  `~/Library/LaunchAgents/{package_info().name}.plist` (`autostart.rs:103-106`,
  removal keyed to the new name at `:98`), so the rename orphans the old
  agent — the migration hook also re-registers autostart under the new
  name when the old agent/.desktop/Run-key entry exists (Linux .desktop
  is productName-derived; Windows Run key is the same class).
- *Allowed "Handy" survivors (exhaustive; basis for AC1/AC2):* LICENSE/
  copyright notices; the attribution line; the legacy portable-marker
  string on read sides (NSIS `:600`, `portable.rs:121` legacy branch);
  `HANDY_DISABLE_UPDATER`; the `"handy_keys"` wire value + the
  module/component filenames above; the external `handy-keys = "0.3.4"`
  crates.io dependency (`Cargo.toml:87` — unrenamable, like catalog ids
  `handy-computer/*` and the `blob.handy.computer`/`handy.computer`
  domains, whose renames would break downloads/docs);
  updater-adjacent release URLs (`portableInstaller.ts:11-12` + test
  fixtures `portableInstaller.test.ts:10/12/20/24`) — inert while the
  updater is off, repointed with a future feed (non-goal 5); the entire
  `HANDY_*` env-var family, all names kept (verified via
  `grep -rn -o "HANDY_[A-Z_]*" src-tauri/src` + build.yml: worker-fault/
  test-injection vars in `engine_supervisor/{supervisor,worker,mod}.rs`,
  `HANDY_TRANSCRIBE_WORKER_EXE/_LOG`, `HANDY_NO_GTK_LAYER_SHELL`
  `overlay.rs:122` — set by build.yml:663 —,
  `HANDY_KEEP_VULKAN_IMPLICIT_LAYERS` `main.rs:20`,
  `HANDY_METAL_RESIDENCY` `lib.rs:630`,
  `HANDY_FORCE_TRANSCRIPTION_FAILURE` `transcription.rs:1130`,
  `HANDY_DEBUG_MIC_READY_DELAY_MS` `actions.rs:494`, `HANDY_VC_REDIST_DIRS`
  build.yml:334, `HANDY_TEST_FLAG_*` `utils.rs`); the native paste class
  names `HandyPasteProvider` (`paste_tx/macos.rs:51`) /
  `HandyPasteTxWindow`/`HandyPasteTx` (`paste_tx/windows.rs:52/:512`) —
  functional ObjC/Win32 registrations, not user-visible, renaming risks
  paste behavior; the external `handy-keys = "0.3.4"`
  `THEME_STORAGE_KEY = "handy.theme"` localStorage key (`theme.ts:19` —
  renaming silently resets users' theme choice, same genre as the wire
  value above); the upstream help URL `SECURE_INPUT_HELP_URL`
  (`SecureInputWarning.tsx:9-10`) — kept while its docs page is the live
  remediation content, repointed when VoxBar has a docs home; `docs/**`;
  historical deserialization fixtures (`settings.rs:1383`, `:1414`);
  historical release notes `src/content/release-notes/{0.9.0,0.9.7}.md`
  (6 hits: 5+1 — Handy-era content; VoxBar-era notes are VoxBar-branded); code
  and dotfile comments, Rust/TS sources and `src-tauri/.gitignore:11`
  alike (verified counts: `build.rs` 21, `supervisor.rs` 19, `worker.rs`
  10, `secure_input.rs` 9, `utils.rs` 8, `main.rs` 5, `UpdateChecker.tsx`
  2 at `:36/:39`); test identifiers/fixtures (`portable.rs`'s
  `handy_test_*` temp dirs at `:132/:143/:153/:171/:181` and marker
  fixtures at `:136/:185` — kept and extended by AC5 — plus `tray.rs:677`'s
  `"handy-1.wav"`); generated files (`Cargo.lock`, `bun.lock`,
  `src/bindings.ts`).

**Acceptance criteria.**
1. Locale sweep: a recursive scan of every string value in all 26
   `translation.json` files (same python3 walk used for verification above)
   reports **zero** occurrences of exact-case "Handy". Case-sensitive by
   design: the one case-insensitive-only hit per locale
   (`settings.debug.updateChecks.lockedDescription`) embeds the
   `HANDY_DISABLE_UPDATER` name kept for compat — exempt.
2. Residual grep is classified: every hit of `grep -ri handy src
   src-tauri index.html package.json .github nix flake.nix` (baseline: 22
   ts/tsx files in `src` alone) falls into a survivor class above or the
   CI/packaging work item — a review checklist, not a zero-hit gate.
3. Release build, network blocked: reaches the tray, transcribes, no
   update-check request; inert check surfaces log an error, no dialog, no
   crash (manual, target machine).
4. `bun run check:translations` passes; 0 locales missing keys.
5. Portable markers: unit tests in `portable.rs` (extend the existing
   `mod tests` at `:125`) assert `is_valid_portable_marker` accepts both
   magic strings, AND the NSIS template at `installer.nsi:600` visibly
   compares both (template inspection — NSIS cannot be cargo-tested);
   manual — a fresh VoxBar portable install redirects Data/ + HF_HOME,
   and an old Handy portable install still detects.
6. `LICENSE` retains the upstream copyright; About shows attribution; the
   selector shows "VoxBar Keys"; stored `"handy_keys"` still parses
   (fixture test untouched).
7. Installed upgrade (manual): VoxBar installed over an existing Handy
   install starts with the old settings, model library, history + linked
   recordings intact (no fresh-store re-onboarding, no model re-download);
   the Debug panel's path rows show the actually-resolved data dir; if
   autostart was enabled under Handy, the new app is registered at login
   and the orphaned old agent is removed.

## 3. F2 — `defaults-24gb` (S)

**User-visible behavior.** Fresh install: model unload timeout defaults to
**2 minutes** (upstream `Min5`, `settings.rs:141` `#[default]`, applied
:958`; no override key in `resources/default_settings.json`, so the Rust
default governs — verified). Recommended preset stays the catalog's rank-1
`parakeet-unified-en-0.6b` (acc 90, streaming, en; default download Q8_0 ≈
697 MB) — the mechanism (`auto_select_model_if_needed`,
`model.rs:1544-1597`; catalog `recommended_rank`) is upstream; v1's whole
preset work is this confirmation, no catalog edit.

**Settings surface.** None new; existing Advanced → model-unload shows
"2 minutes" selected. Stored settings untouched — a user with `min5` stored
keeps `min5`.

**Memory-pressure behavior.** After 2 idle minutes the worker unloads and
its RSS is freed — that is the feature.

**Acceptance criteria.**
1. Unit test: `ModelUnloadTimeout::default() == Min2`; serde wire string
   `"min2"` (`settings.rs:135-137`; `ModelUnloadTimeout.tsx` verified).
2. Existing v0.9 fixture test (`settings.rs:1383` parses explicit `"min5"`;
   suite at `:1336`) passes unchanged.
3. Fresh install (manual): rank-1 downloaded, 2 idle minutes → the tray's
   **"Unload model" item is disabled** — the residency signal
   (`tray.rs:549-556` gates on `model_loaded`; the label tracks
   `selected_model`, `:533-547`, and does not change on unload).

## 4. F3 — `memory-pressure-gate` (M)

**User-visible behavior.** Loading a model whose forecast footprint exceeds
available RAM is **refused before any load starts**: the menu stays usable,
the current model stays resident, and the existing failure toast appears
(`loading_failed` → `App.tsx:197-215`, `errors.modelLoadFailed`)
carrying "Not enough free memory for <model>: needs ~X GB, ~Y GB free
(guard can be disabled in Settings)" — Rust-side text in the
untranslated description slot, like every other load failure today.

**Settings surface.** `memory_pressure_guard: bool`, default **true**, in
Settings → Advanced next to model-unload, per the 7-step recipe (research-ui
§2.6): `settings.rs` field+default → `change_*` command (`shortcut/mod.rs`
pattern) → `collect_commands!` (`lib.rs:652`) → debug-build regen of
`src/bindings.ts` (`lib.rs:776-784`) → `settingUpdaters`
(`settingsStore.ts:83`) → toggle + i18n keys (C3).

**Mechanism.**
- `memory.rs` (today glibc-only tuning, macOS no-op — read in full) gains
  `available_memory_bytes() -> Option<u64>`: macOS **`os_proc_available_memory()`**
  (bytes available to the process — NOT a naive `host_statistics64`
  `free_count`, which is a few hundred MB on a healthy Mac by design and
  would refuse nearly every load at 1.5 GiB headroom; fall back to the
  `host_statistics64` sum free+inactive+purgeable+speculative only if the
  symbol is unavailable); Linux `/proc/meminfo MemAvailable`; Windows
  `GlobalMemoryStatusEx ullAvailPhys`.
- Pure decision fn `gate_should_refuse(free: Option<u64>, forecast: u64,
  headroom: u64) -> bool`; forecast = `ModelInfo.size_mb` MiB (`model.rs:67`)
  — an estimate (GGUF disk ≠ resident RAM, research-models §8); `headroom`
  is a named constant, default **1.5 GiB**.
- Gate sits in `load_model_with_device` (`transcription.rs:527`; `load_model`
  delegates at `:518-519` and the only other caller is the CLI benchmark, so
  the gate covers every GUI load) **before** the old-engine drop
  (`:590-602`), crediting the outgoing model: `effective_free = free +
  resident_footprint` (peak = max(old, new) once the drop precedes the
  build). `resident_footprint` = measured worker RSS for TranscribeCpp
  (pid, `supervisor.rs:1063`); the same `size_mb`-derived estimate F4 uses
  for in-process ONNX engines (no pid — `OnnxEngine`,
  `transcription.rs:183-194`); 0 when nothing resident or no estimate.

**Memory-pressure errors & fallback.** Probe returns `None` (unsupported/OS
error) → **fail-open**: log a warning and proceed — a broken probe must
never brick model loading. Toggle off → gate skipped entirely. Refusal is
the only failure mode that reaches the user.

**Acceptance criteria.**
1. Table-driven unit tests for `gate_should_refuse`: `None` → false
   (fail-open); boundary `forecast + headroom == free` → allow, `== free+1`
   → refuse; resident-credit arithmetic incl. the ONNX estimate branch.
2. Unit test: `memory_pressure_guard` default true; serde round-trip both
   values; old settings JSON without the field parses to the default.
3. Unit test (macOS runner): `available_memory_bytes()` returns `Some(n)`
   with `0 < n <= hw.memsize`.
4. False-positive guard, the must-not-break path: manual on the idle
   target machine with the guard ON, rank-1 (~697 MB) loads with no
   refusal, and the probe reads ≥ 8 GiB available on an idle 24 GB
   machine — a lower reading means the probe undercounts (AC3's
   `0 < n <= hw.memsize` is true under any undercount and cannot catch
   this).
5. Manual (target machine, pressure induced): selecting a large model
   (e.g. F16 multi-GB quant) → toast, no load attempt, existing model keeps
   transcribing; toggle off → load proceeds (may OOM, user asked for it).
6. Linux/Windows: probe compiles; untestable-in-CI tiers fail open (AC 1
   covers the logic; probe AC is macOS-only as stated).

## 5. F4 — `tray-model-ram-status` (M)

Display-only (unload ships upstream — `tray.rs:549-556` + `lib.rs:303-311`).

**User-visible behavior.** The tray model submenu label and tooltip show the
**resident** model and its approximate footprint: `Parakeet Unified EN 0.6B —
697 MB` (measured) or `… — ~697 MB` (estimate, ONNX in-process models) —
fixing the selected-vs-resident divergence (`MenuInputs` carries only
`model_loaded: bool` + `selected_model`, `tray.rs:54-66`; label built from
`selected_model`, `:533-547`). Refreshed on load/unload only
(`model-state-changed` → `update_tray_menu`, `lib.rs:355-359`); no polling.

**Settings surface.** None.

**Mechanism.** `MenuInputs` gains `resident_model: Option<(String, String)>`
(id + display name) AND `model_ram: Option<String>` (pre-formatted;
`PartialEq` at `tray.rs:53` already rebuilds only on change). The submenu
label derives from `resident_model` when present, falling back to today's
`selected_model` lookup (`tray.rs:533-547`) — without the field the label
still tracks the selection and the claimed fix is vaporware. TranscribeCpp:
worker RSS from the tracked pid (`supervisor.rs:1063`, needs an accessor);
`LoadedInfo` (`protocol.rs:144-156`) has no memory field and v1 does not
change the wire protocol. ONNX in-process (`transcription.rs:183-194`):
estimate from `size_mb`, `~`-labeled — the same source F3's
`resident_footprint` uses.

**Memory-pressure errors & fallback.** RSS read fails → fall back to the
`~`-estimate; no estimate available → omit the segment entirely. Menu build
never fails on measurement errors.

**Acceptance criteria.**
1. Unit test: formatting fn (measured vs estimate vs absent) and
   `MenuInputs` inequality driving a rebuild.
2. Unit test: label resolution prefers `resident_model` over
   `selected_model` (failed switch, pending unload — the states the
   divergence produces).
3. Locale surface: the RAM segment and model names are pre-formatted
   Rust-side, and tray strings are build-time codegen from the locale
   "tray" section (`tray_i18n.rs:4-13` — verified), so v1 needs **zero**
   new `tray.*` keys; assert `bun run check:translations` reports no key
   drift, and any key an implementer does add lands in all 26 locales
   (C3).
4. Manual (target machine): load rank-1 → label within ±25% of Activity
   Monitor's worker RSS; unload → segment disappears.

## 6. F5 — `apple-silicon-verdict` (decision record, no code)

Highest-accuracy local ASR on Apple Silicon = upstream transcribe-cpp
static Metal (`Cargo.toml:157-159`) + catalog GGUF (verified: cohere-2026
92, granite-4.1-2b 92, parakeet-1.1b 91, whisper-large-v3 89; 69 models);
the built-in ONNX parakeet is CPU-only on macOS (no `coreml` in
`Cargo.toml`). **No parakeet-mlx sidecar in v1** — evidence in
`research.md` §5; revisit only on a concrete gap. No AC — this section
makes the non-goal binding.

## 7. Non-goals (explicit)

Upstream already ships (do not rebuild; proofs in research.md §dropped):
tray "Unload model now" (`tray.rs:549-556`, `lib.rs:303-311`); the
recommended-model preset mechanism (`model.rs:1544-1597`); the
parakeet-mlx sidecar (F5).

Fork-level non-goals for v1:
4. Auto-select-smaller-model fallback when the gate refuses — refuse-with-
   warning suffices; fallback (reusing `get_available_models` rank order)
   is a later-version stretch.
5. A VoxBar update feed, signing keys, or any update delivery — v1 disables
   the updater outright (F1); hiding the inert check surfaces goes with it.
6. Renaming catalog model ids `handy-computer/*` — HuggingFace download
   paths; renaming breaks all one-click downloads.
7. Renaming the `"handy_keys"` wire value, module, or generated command
   names (F1 decision); translating "VoxBar".
8. Continuous RAM polling / tray refresh timers — violates C2.
9. GPU/device selection UI, new engines, model conversions, UI redesign,
   Android/iOS targets, new locales, catalog edits beyond F2's rank-1.

## 8. Verification basis & not-run disclosure

All citations were read/run at `handy-dictation/` (HEAD `c8262ab`) across
the authoring session and four independent review passes — every count,
grep, and parse matching (round 4 added: the workflows/flake/nix greps,
the `HANDY_*` family enumeration, `shortcut/mod.rs:563-566`,
`paste_tx/{macos,windows}.rs` class names, `tauri.conf.json:73`,
`history.rs:79-80`, `settings.rs:878/:1039/:1250`, `lib.rs:886`,
`autostart.rs:98/:103-106`, `tray_i18n.rs:4-13`, the commented-out
`[[bin]]` at `Cargo.toml:22-25`, release-notes recount 5+1=6).
**Not run:** any build, `cargo test`, `bun run check:translations`, the app.
