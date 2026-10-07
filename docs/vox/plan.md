# VoxBar v1 — Implementation Plan

Input: `docs/vox/spec.md` (binding), `docs/vox/research.md`, the codebase at
`handy-dictation/` (upstream `c8262ab` — confirmed via `git log --oneline -1`
this session). Every code citation below was re-read in this planning
session unless attributed to the spec/research; this revision incorporates a
review pass whose seven findings were each re-verified against the code
before adoption. Paths relative to `handy-dictation/`.

Baseline checks run in this planning session: `bun run check:translations` →
green ("All 25 languages have complete translations"); python3 recursive
value scan of all 26 `translation.json` → exactly 17 exact-case "Handy"
values in 24 locales, 18 in `nl`, 16 in `uk` (matches spec AC1's counts).
NOT run by planning: `cargo check`, `cargo test`, any tauri build — every
compile/test acceptance below rests on the implementing task's own
execution.

Build order follows the spec (§1): F1 → F2 → F3 → F4. F5
(`apple-silicon-verdict`) is a decision record already embodied in the spec
+ research notes — no code task. Rebranding and build configuration come
first (T1, T2), per the ordering rule; T1 exists so that every later cargo
invocation — including the release build, which tauri drives and which
cannot take `-j` flags — is parallelism-capped on this memory-constrained
machine. T2's lib-crate rename forces a full rebuild, so the cap must land
before it.

T2/T3 split: the spec's F1 is one feature, but two commits. T2 is the
identity sweep (mechanical, wide); T3 is the installed-upgrade data
continuity hook (new logic, depends on the new identifier from T2). Each is
independently committable and testable.

The spec's survivor list (§F1 "Allowed Handy survivors") is the authoritative
checklist for T2's residual-grep classification and is not duplicated here.

Two deliberate deviations from the spec's letter, both review-driven and
code-verified:
- **Spec names `DebugPaths.tsx:29/37/46` for backend-resolved paths** — the
  component is dead code (grep this session: `grep -rn 'DebugPaths' src/
  tests/` matches only its own definition; `DebugSettings.tsx` does not
  import it). Editing it is unobservable; T3 instead asserts the *live*
  path rows (About's `AppDataDirectory`/`LogDirectory`,
  `AboutSettings.tsx:80-81`, already backend-resolved).
- **Spec's attribution wording** implies an existing line at
  `AboutSettings.tsx:74` — that line is the source-link button's
  `openUrl("https://github.com/cjpais/Handy")` onClick, not an attribution
  string; no "fork of"/"CJ Pais" string exists in `src/` or the locales.
  T2 CREATES the attribution line; the localization decision is made below
  (eslint-disabled literal, not an i18n key).

---

## T1 — `build-parallelism-cap` (S)

Cap cargo build parallelism at 8 jobs so no later cargo invocation — the
debug builds, `cargo test`, and the tauri-driven release build (which shells
into cargo and cannot take `-j` flags) — spawns an unbounded number of rustc
processes on the operator's memory-constrained machine.

**Files**
- `handy-dictation/.cargo/config.toml` — exists on disk today containing only
  an empty `[build]` header (read this session); add `jobs = 8` under it.

Cargo discovers config by walking up from the working directory, so any
invocation from `src-tauri/` (where tauri runs cargo) finds the parent
`handy-dictation/.cargo/config.toml`. The file is already git-tracked (git
status clean apart from `docs/vox/`), and `.cargo` is not ignored by
`src-tauri/.gitignore` (read this session).

**Acceptance**
1. `.cargo/config.toml` contains `[build]` with `jobs = 8`.
2. A `cargo check` from `src-tauri/` succeeds — cargo fails fast on an
   unparseable/discovered-broken config, so a green check proves the file is
   discovered and parsed (the direct `cargo config get build.jobs` command is
   nightly-only: verified this session — `cargo 1.99.0` stable rejects it).
3. Best-effort direct observation: during a clean `cargo check`, repeated
   `pgrep -c rustc` samples never exceed 8.

**Test approach**: `cd src-tauri && cargo check` (also warms the target dir
for T2); sample `pgrep -c rustc` in a second shell during the compile.

---

## T2 — `voxbar-rebrand` (M) — spec F1, identity sweep

Replace every user-visible Handy identity surface; change no runtime
behavior beyond disabling the updater. The installed-upgrade data migration
is T3, not this task.

**Files (exact)**
- Identity/version trio: `src-tauri/tauri.conf.json` (`productName` →
  "VoxBar" `:3`; `identifier` → `"com.voxbar.app"` `:5`; deb + rpm
  `/usr/lib/Handy` → `/usr/lib/VoxBar` (~`:50`,`:62`); Windows signCommand
  `-d Handy` → `-d VoxBar` (~`:73`); **remove** `bundle.createUpdaterArtifacts`
  (~`:28`) and the whole `plugins.updater` pubkey/endpoints block (end of
  file — verified this session)); `src-tauri/Cargo.toml` (`name`/`description`
  `:2-5`, `default-run` `:8`, lib name `handy_app_lib` → `voxbar_app_lib`
  `:19`; **keep** `handy-keys = "0.3.4"` at `:87` and the commented-out
  `[[bin]]` block `:22-25`); `package.json` (`"name"` → `"voxbar-app"` `:2`).
- Lib rename use sites: `src-tauri/src/main.rs` `:5/:20/:29/:30/:34` and
  `src-tauri/src/audio_toolkit/bin/cli.rs:4` (the latter sits in the
  commented-out `[[bin]]` target — **not compiled, a compile error will not
  catch it**; grep, don't rely on the build).
- Window/tray/CLI titles: `src-tauri/src/lib.rs:948` (`.title("Handy")`);
  `src-tauri/src/tray.rs:442-452` (tooltip + `version_label()` "Handy v{}" →
  "VoxBar v{}"); `src-tauri/src/cli.rs:5` (clap `name = "voxbar"`,
  `about = "VoxBar — Speech to Text"`); `index.html:6` (`<title>`).
- Linux rpath moves **together with** the deb/rpm mapping:
  `src-tauri/build.rs:19` (`-Wl,-rpath,$ORIGIN/../lib/Handy` →
  `/usr/lib/VoxBar`).
- Brand assets, regenerated as neutral placeholders, **filenames kept**
  (tray selection code keys on filenames, `tray.rs:191-215` per research):
  all of `src-tauri/icons/**` and
  `src-tauri/resources/{handy,handy_warning,tray_*}.png`;
  `src/components/icons/HandyHand.tsx` + `HandyTextLogo.tsx` (replace the
  glyph content with a neutral VoxBar mark; component filenames may stay).
- Visible strings: `src/components/settings/debug/KeyboardImplementationSelector.tsx:11`
  (label "Handy Keys" → "VoxBar Keys" — actual path verified this session,
  the component lives under `settings/debug/`);
  `src-tauri/src/shortcut/mod.rs:563-566` (toast "Failed to initialize
  VoxBar Keys: {}. Reverted to Tauri." — **keep** the `"handy_keys"` serde
  wire value (`settings.rs:217`), the `shortcut/handy_keys.rs` module, and
  the `HandyKeysShortcutInput.tsx` filename).
- Attribution (**created, decision**): `src/components/settings/about/AboutSettings.tsx`
  — add "VoxBar is a fork of Handy by CJ Pais (MIT)" as a new
  `SettingContainer` row (e.g. under acknowledgments) rendered as an
  **eslint-disabled literal** following the component's existing
  untranslated-literal pattern at `:50-51` (`{/* eslint-disable-next-line
  i18next/no-literal-string */}` before the `v{version}` span — verified this
  session). NOT an i18n key: the line necessarily contains "Handy", and a
  locale value containing exact-case "Handy" would break AC1's zero-Handy
  locale scan; an untranslated key in all 26 locales would violate the
  values-only locale rule for no benefit. The source-link button's onClick
  at `:74` (`openUrl("https://github.com/cjpais/Handy")`) stays pointed at
  upstream.
- Locales: all 26 `src/i18n/locales/*/translation.json` — rewrite every
  exact-case "Handy"-valued **value** to "VoxBar" (verified baseline this
  session: 17 keys in 24 locales, 18 in `nl`, 16 in `uk`). **Values only —
  never add/rename keys** (`check:translations` gates key parity; C3).
  Script the sweep (the python3 recursive walk from the spec's
  verification), don't hand-edit 26 files.
- Portable marker, three sites one decision: `src-tauri/nsis/installer.nsi`
  — read side `:600` gains `${OrIf} $2 == "VoxBar Portable Mode"` alongside
  the existing "Handy Portable Mode" comparison (verified this session),
  write side `:771` writes the new string; `src-tauri/src/portable.rs` —
  `is_valid_portable_marker` `:118-122` accepts both strings, legacy
  upgrade write `:34` writes the new one. Extend the existing `mod tests`
  (starts `:125`) to cover both markers.
- CI/packaging (same change or the first post-rename CI run breaks):
  `.github/workflows/build.yml` — `default` input `:22`, deb remap `:398`,
  macOS otool checks `:590/:593` (pin to the `voxbar` binary, drop the
  `|| true`), Linux smoke `:663-666` (`LD_LIBRARY_PATH` `/usr/lib/Handy` →
  `/usr/lib/VoxBar`; **keep** `HANDY_NO_GTK_LAYER_SHELL=1`), package
  assertions `:675-707` (`usr/bin/handy$` → `usr/bin/voxbar$`,
  `/usr/lib/Handy/lib*` → `/usr/lib/VoxBar/lib*`), **and the Windows
  package audit** `:835` (`Get-ChildItem $Root -Filter "handy.exe"` →
  `voxbar.exe`) plus the launch at `:842` (`& $handy.FullName
  --list-devices` — rename the variable or the filter; both verified this
  session); **keep** the `blob.handy.computer` ORT URLs `:366/:381` and
  `HANDY_VC_REDIST_DIRS` `:334`. `.github/workflows/release.yml:77`,
  `build-test.yml:41`, `pr-test-build.yml:47` — asset prefixes `handy` →
  `voxbar`. `.github/workflows/nix-check.yml:94/:128-130` — attrs follow
  the flake rename; **keep** the cache name at `:53`.
- Nix, all references renamed **together** (pname/mainProgram at
  `flake.nix:90/:183` drive the rest — verified this session):
  `flake.nix:188` (`default = self.packages.${system}.handy` → `.voxbar`),
  `flake.nix:197` (`programs.handy.package = … .handy` → `programs.voxbar.package
  = … .voxbar`), `flake.nix:205` (`services.handy.package = … .handy` →
  `services.voxbar.package = … .voxbar`), the option namespaces themselves —
  `programs.handy` → `programs.voxbar` in `nix/module.nix:25/:28` and
  `flake.nix:196-198`; `services.handy` → `services.voxbar` in
  `nix/hm-module.nix:13-27` (incl. `defaultText` "handy.packages…"
  at `:21`) and `flake.nix:204-206`; and `nix/hm-module.nix:34`
  (`ExecStart = "${cfg.package}/bin/handy"` → `/bin/voxbar` — line verified
  this session). Renaming the option namespaces is an API break for
  existing nix users — acceptable for a personal fork with none. No
  hash-bump hazard from the crate rename: the flake pins
  `cargoLock.lockFile = ./src-tauri/Cargo.lock` (`flake.nix:98-101`,
  verified this session — grep finds no `cargoHash` anywhere), so the
  Cargo.lock root-package rewrite needs no nix hash update. **CI never
  evals `packages.default` or the nixos/hm modules** (nix-check.yml checks
  only the attrs at `:94/:128-130`), so a miss here breaks silently:
  verification is local (`nix eval .#packages.$system.default` /
  `nix build .#default` + `nix flake check`, if nix is available on the
  dev machine) **plus** a mandatory `grep -rn 'handy' flake.nix nix/`
  zero-unclassified-hits pass recorded in the PR body.
- Docs identifier references (README.md and BUILD.md sit at the repo
  root — **outside** the spec's `docs/**` survivor class and outside the
  original AC2 grep scope, so without this item nothing renames or
  classifies them): `README.md:299-301/:309/:312` — the user-facing
  data-dir table + `mkdir`/`New-Item` model-dir examples; post-migration
  they would point at the exact legacy dir T3 migrates away from — and
  `BUILD.md:159-160` — the `osascript … application id "com.pais.handy"`
  quit helper and `tccutil reset Accessibility com.pais.handy` — replace
  `com.pais.handy` with `com.voxbar.app` (all occurrences verified by
  whole-tree grep this session). The remaining Handy-brand
  prose/screenshots/badges in README.md/BUILD.md are classified in the
  PR-body checklist as docs-adjacent survivors (same class as `docs/**`);
  `docs/troubleshooting/ubuntu-26-04-gnome-wayland/README.md:44-45`'s
  identifier references stay under the spec's `docs/**` survivor class
  (historical Handy-era troubleshooting).
- Unchanged (survivors, per spec §F1): `LICENSE`, the `HANDY_*` env-var
  family incl. `HANDY_DISABLE_UPDATER` (`settings.rs:1231-1238` — updater
  disable is config-removal only, this env path stays for packaging compat),
  the inert update-check surfaces (tray item + Settings toggle stay
  visible-and-inert; `UpdateChecker.tsx:108-109` already swallows the
  failure), catalog ids `handy-computer/*`, `handy.computer` domains,
  `THEME_STORAGE_KEY = "handy.theme"`, `docs/**`, historical fixtures and
  release notes, code comments, generated files (`Cargo.lock`, `bun.lock`,
  `src/bindings.ts`), and the autostart plugin init
  (`lib.rs:888-890` — `MacosLauncher::LaunchAgent` is a strategy enum, not
  a name string; verified this session).

**Acceptance (spec F1 AC1–AC6; AC7 moves to T3)**
1. Python3 recursive scan of every string value in all 26
   `translation.json` reports **zero** exact-case "Handy" (baseline
   confirmed this session; the case-insensitive-only hit embedding
   `HANDY_DISABLE_UPDATER` is exempt — and the new attribution literal in
   `AboutSettings.tsx` is outside the locale files by design, see the
   decision above).
2. Every hit of `grep -ri handy src src-tauri index.html package.json
   .github nix flake.nix README.md BUILD.md` is classified against the
   spec's survivor list or this task's work items — recorded as a review
   checklist in the PR body (README.md/BUILD.md are in scope so their
   identifier refs are renamed and their brand prose is explicitly
   classified, not silently uncovered); additionally `grep -rn handy
   flake.nix nix/` and `grep -rn handy .github/workflows/build.yml` show
   zero unclassified hits (covers the silent-break nix/Windows-audit
   surfaces CI cannot catch).
3. `bun run check:translations` passes; 0 locales missing keys.
4. `cd src-tauri && cargo test` green, including the extended portable
   marker tests (both magic strings accepted) and the **untouched** v0.9
   fixture suite (`settings.rs:1336+`, `"handy_keys"` at `:1414` still
   parses).
5. `grep -rn handy_app_lib src-tauri/src src-tauri/src/audio_toolkit` →
   zero hits (catches the non-compiled `cli.rs:4`).
6. Template inspection: `installer.nsi:600` visibly compares both magic
   strings (NSIS cannot be cargo-tested).
7. `grep -n 'createUpdaterArtifacts\|plugins.updater\|"pubkey"'
   src-tauri/tauri.conf.json` → zero hits.
8. Manual, batched target-machine + Windows pass (see below): (a) release
   build, network blocked: reaches the tray, transcribes, no update-check
   request, inert check surfaces log an error with no dialog and no crash
   (spec AC3); (b) **spec AC5's portable tail, assigned here**: a fresh
   VoxBar portable install redirects `Data/` + sets `HF_HOME`, and an old
   Handy portable install still detects. Platform caveat: portable mode is
   NSIS/Windows (`installer.nsi`); the M4 Pro cannot run it — if no Windows
   machine is available this item is **recorded as not-run**, with the
   extended portable unit tests + the `:600` template inspection standing
   as the automated coverage.

**Test approach**: `cd src-tauri && cargo test` (portable + settings
suites); `bun run check:translations` from `handy-dictation/`; the python3
locale-value scan; the classification greps (incl. the nix/ and build.yml
zero-unclassified passes); `cargo check` for compile sanity of the rename;
local `nix eval`/`nix flake check` if nix is available. AC8a is the manual
release pass on the M4 Pro; AC8b is the Windows portable pass.

---

## T3 — `legacy-data-migration` (M) — spec F1, installed-upgrade continuity

T2 renames `productName` + `identifier`, so a VoxBar build installed over an
existing Handy install resolves a new app-data dir and would otherwise start
fresh (re-onboarding, model re-download, lost history). Portable installs
are unaffected (`Data/` sits next to the exe; `portable.rs:83-87`).

**Files (exact)**
- NEW `src-tauri/src/legacy_migration.rs` — on first run, when the new data
  dir holds no settings store and a legacy Handy dir exists, migrate
  per-item and idempotently: `settings_store.json`
  (`SETTINGS_STORE_PATH`, `settings.rs:877` — verified this session),
  `models/`, `history.db` **and** `recordings/` together
  (`managers/history.rs:78-80` — verified: both resolve under
  `portable::app_data_dir`), logs (best-effort: plugin-init logging may have
  already created the new log file before the hook runs — skip-if-present
  semantics simply leave old logs in place). Skip items already present;
  prefer same-volume rename; never delete the source dir on partial failure
  (log + retry next launch). Pure decision fns (which items, skip-if-present,
  source-dir resolution, legacy-entry paths) kept separate from the io
  walker for testability (C4). The legacy dir derivation hardcodes the
  legacy identity constants (`"Handy"`, `"com.pais.handy"`) — in **code
  and config** the old identifier's only occurrence is
  `tauri.conf.json:5`, which T2 rewrites (whole-tree grep this session:
  every other occurrence is documentation — README.md/BUILD.md, covered
  by T2's docs-identifier item, and `docs/troubleshooting/**` +
  `docs/vox/**`, survivors) — so the constant is clean to pin in this
  module.
- `src-tauri/src/lib.rs` — `mod` declaration + the migration as the
  **FIRST statement(s) of the `.setup()` closure (`lib.rs:893`)** — before
  `specta_builder.mount_events(app)` (`:901`), before the headless branch
  (`if headless_mode {` at `:910`), and before the
  `let mut settings = get_settings(app.handle());` read (`:987`). Placement rationale (all verified this session):
  - Not beside `portable::init()` (`:646`): no `AppHandle` exists there —
    the Builder chain starts at `:652`, `.build(generate_context!())` runs
    at `:1095` — and the data dirs resolve only via
    `portable::app_data_dir(app: &AppHandle)` (`portable.rs:83-87`); a
    pre-builder hook would hand-roll both per-OS dir derivations, exactly
    the unverifiable identifier-vs-productName derivation flagged in
    Global risk 1. In `.setup()` the handle exists and
    `portable::app_data_dir` gives the new dir through the same code the
    app itself uses.
  - **First statement, not merely "before `initialize_core_logic`
    (`:1013`)"** — that weaker constraint is the task's silent-failure
    mode: `lib.rs:987` already runs `get_settings(app.handle())` inside
    `.setup()` BEFORE `initialize_core_logic`, and on a fresh store
    `get_settings` **writes** the defaults
    (`store.set("settings", …)`, `settings.rs:1070-1073`) — creating
    `settings_store.json` in the new dir and defeating the no-store
    trigger (`lib.rs:350` is only the first read *inside*
    `initialize_core_logic`, not the first overall). The headless
    branch's `ModelManager::new` (`lib.rs:913-914`) `create_dir_all`s the
    new `models/` dir (`model.rs:561-563`), defeating skip-if-present on
    `models/`. Any later placement lets the migration silently no-op →
    re-onboarding + model re-download, discovered only in the manual AC.
- Autostart, re-scoped to the live mechanism (all verified this session):
  - macOS 13+ uses **SMAppService** (`apply_autostart` →
    `macos::set_login_item(enabled)`, `autostart.rs:21-27`), registered
    against the app bundle itself. **Re-registration under the new name
    needs no code**: once `settings_store.json` migrates,
    `initialize_core_logic` → `apply_autostart(app_handle,
    settings.autostart_enabled)` (`lib.rs:363`) registers the new bundle on
    first launch.
  - The every-launch legacy cleanup `remove_plugin_launch_agent`
    (`autostart.rs:94-107`) removes
    `~/Library/LaunchAgents/{package_info().name}.plist` — keyed to the
    **new** name post-rename, so it can never remove the old
    `Handy.plist`. The migration explicitly removes the old-name plist
    (`~/Library/LaunchAgents/Handy.plist`) when present — this covers only
    pre-SMAppService Handy installs.
  - **Documented residual, not code-fixable**: a current-version upgrade
    (Handy 0.9.8 → VoxBar on macOS 13+, the M4 Pro's path) leaves the old
    bundle's SMAppService registration orphaned — SMAppService manages only
    its own bundle, so the new app cannot unregister it. Removal requires
    uninstalling the old app or System Settings → General → Login Items.
    This goes in the migration's log output and the release notes, and the
    AC below asserts it as documented-orphaned, not removed.
  - Linux `.desktop` / Windows Run-key legacy entries are the same
    explicit-removal class where the old name is derivable; the migration's
    decision fn exposes per-OS legacy paths, best-effort on non-macOS
    tiers (the operator's machine is macOS; untestable tiers log and skip).
- DROPPED from this task (review finding, verified): the spec's
  `DebugPaths.tsx:29/37/46` work item — the component is dead code
  (`grep -rn 'DebugPaths' src/ tests/` matches only its own definition;
  `DebugSettings.tsx` does not import it), so replacing its hardcoded
  `%APPDATA%/handy*` strings is unobservable. The live, backend-resolved
  path rows already exist — About renders `AppDataDirectory` +
  `LogDirectory` (`AboutSettings.tsx:80-81`, verified) backed by
  `commands.get_app_dir_path` (`commands/mod.rs:34-37`). The AC below
  asserts those rows. If `DebugPaths` is ever wired into `DebugSettings`
  later, its strings must be fixed in that change (noted in the survivor
  checklist).

**Acceptance**
1. Unit tests (temp dirs, no app handle): per-item skip-if-present;
   idempotent second run is a no-op; partial-failure path leaves the source
   dir intact; item selection fn returns exactly the item set above;
   source-dir resolution given (new_dir, home) returns the legacy dir or
   None.
2. Unit test: legacy-entry decision fn — given home + old product name, the
   macOS legacy plist path resolves (`~/Library/LaunchAgents/Handy.plist`)
   and exists → remove, absent → no-op. (No "re-register" case:
   re-registration is automatic via settings migration + `apply_autostart`
   at `lib.rs:363`.)
3. `cd src-tauri && cargo test` green; `bun run check:translations` green
   (no new keys expected — this task adds no user-visible strings; the
   migration logs in English like other Rust-side diagnostics).
4. Manual, target machine (spec AC7, batched with T2 AC8a): VoxBar
   installed over an existing Handy install starts with old settings, model
   library, history + linked recordings intact (no re-onboarding, no model
   re-download); **About's path rows** (`AppDataDirectory`/`LogDirectory`)
   show the actually-resolved (migrated) data dir; if autostart was enabled
   under Handy, the new app is registered at login (automatic via migrated
   `autostart_enabled` + `apply_autostart`); the legacy
   `~/Library/LaunchAgents/Handy.plist` is removed when it existed; the
   old bundle's SMAppService registration is **present and active until
   the old app is removed or the entry is toggled off in System Settings**
   (while the old Handy.app remains installed, the registration is
   functional — the old app launches at login; it is not "inert", and a
   two-apps-at-login outcome must NOT be accepted as passing) — verified
   as such in System Settings, recorded as the documented orphan.

**Test approach**: `cd src-tauri && cargo test legacy_migration`; then the
manual installed-over-Handy pass on the M4 Pro (build with `bun run tauri
build`, install over a populated Handy install).

---

## T4 — `defaults-24gb` (S) — spec F2

Fresh installs default the model-unload timeout to 2 minutes. Stored
settings are untouched — a user with `min5` stored keeps `min5`. The
recommended preset stays the catalog's rank-1 `parakeet-unified-en-0.6b`
(default download Q8_0 ≈ 697 MB) — the mechanism is upstream
(`auto_select_model_if_needed`, `model.rs:1544`; `default_quant_file`,
`model.rs:142-148`; both verified this session); **no catalog edit, no
onboarding change**.

**Files (exact)**
- `src-tauri/src/settings.rs` — move `#[default]` from `Min5` to `Min2` in
  `ModelUnloadTimeout` (enum at ~`:133-143`, `#[default]` on `Min5`
  verified this session; default applied via
  `ModelUnloadTimeout::default()` at ~`:958`). No override key exists in
  `resources/default_settings.json` (spec: verified) — the Rust default
  governs. Nothing else.

**Acceptance (spec F2 AC1–AC2; AC3 is the manual tail batched into T6)**
1. New unit test: `ModelUnloadTimeout::default() == Min2` and the serde wire
   string round-trips `"min2"` (mind the documented trap: generated TS
   bindings mislabel wire strings — the frontend already sends `"min2"`
   (`ModelUnloadTimeout.tsx:30-46`, read this session)).
2. The existing v0.9 fixture suite (`settings.rs:1336+`, explicit `"min5"`
   at `:1383`) passes **unchanged**.
3. `cd src-tauri && cargo test` green.

**Test approach**: `cd src-tauri && cargo test settings` then the full
`cargo test`. Manual fresh-install check (rank-1 downloads; after 2 idle
minutes the tray "Unload model" item disables — `tray.rs:549-556` gates on
`model_loaded`, verified this session) batches with T6's manual pass.

---

## T5 — `memory-pressure-gate` (M) — spec F3

Refuse model loads whose forecast footprint exceeds available RAM, before
any load starts; settings toggle, default ON; fail-open on probe failure.

**Files (exact)**
- `src-tauri/src/memory.rs` — today glibc-only tuning, macOS no-op (read in
  full this session). Add:
  - `available_memory_bytes() -> Option<u64>` — macOS
    `os_proc_available_memory()` (NOT a naive `host_statistics64`
    `free_count`; fall back to the free+inactive+purgeable+speculative sum
    only if the symbol is unavailable), Linux `/proc/meminfo MemAvailable`,
    Windows `GlobalMemoryStatusEx ullAvailPhys`.
  - Pure `gate_should_refuse(free: Option<u64>, forecast: u64, headroom: u64)
    -> bool` and `const` headroom default **1.5 GiB**. Forecast =
    `ModelInfo.size_mb` MiB (`model.rs:67`, verified this session) — an
    estimate (GGUF disk ≠ resident RAM, research-models §8).
  - `rss_bytes_for_pid(pid) -> Option<u64>` helper (macOS
    `proc_pid_rusage` on our own child; Linux `/proc/<pid>/statm`; Windows
    `GetProcessMemoryInfo`) — `None` on any failure.
- `src-tauri/src/engine_supervisor/supervisor.rs` — public accessor for the
  worker pid (`struct Control { pid: u32, ... }` at `:1063` is private —
  verified this session).
- `src-tauri/src/managers/transcription.rs` — the gate at the top of
  `load_model_with_device` (`:527`, verified) **before** the old-engine drop
  (`:590-602`, verified), crediting the outgoing model:
  `effective_free = free + resident_footprint` where the footprint is
  measured worker RSS (TranscribeCpp, pid accessor) or the same
  `size_mb`-derived estimate for in-process ONNX engines
  (`OnnxEngine`, `:183-194`, verified), 0 when nothing resident. Refusal
  reuses the existing `loading_failed` emit path (frontend toasts,
  `App.tsx:197-215` per spec) with "Not enough free memory for <model>:
  needs ~X GB, ~Y GB free (guard can be disabled in Settings)". Probe
  returns `None` → log + proceed (fail-open). Toggle off → gate skipped.
- Settings surface, the 7-step recipe: `src-tauri/src/settings.rs` —
  `memory_pressure_guard: bool`, default **true**;
  `src-tauri/src/shortcut/mod.rs` — `change_memory_pressure_guard_setting`
  command (the `change_*` settings commands live here —
  `change_audio_feedback_setting` at `:600`, verified this session);
  `src-tauri/src/lib.rs` — register in `collect_commands!` (`:652`,
  verified); regen `src/bindings.ts` via a debug build (`lib.rs:776-784`,
  verified — export is `#[cfg(debug_assertions)]` only);
  `src/stores/settingsStore.ts` — `settingUpdaters` entry (map at `:83`,
  verified); NEW `src/components/settings/MemoryPressureGuard.tsx` toggle,
  wired into `src/components/settings/advanced/AdvancedSettings.tsx` next to
  `ModelUnloadTimeoutSetting` (`:40`, verified this session);
  `src/i18n/locales/*/translation.json` ×26 — new keys in every locale (C3).

**Acceptance (spec F3 AC1–AC3, AC6; AC4/AC5 manual)**
1. Table-driven unit tests for `gate_should_refuse`: `None` → false
   (fail-open); boundary `forecast + headroom == free` → allow, `== free+1`
   → refuse; the resident-credit arithmetic including the ONNX
   estimate branch.
2. Unit tests: default is true; serde round-trip both values; old settings
   JSON without the field parses to the default.
3. Unit test (macOS): `available_memory_bytes()` returns `Some(n)` with
   `0 < n <= sysctl hw.memsize`.
4. `bun run check:translations` passes — new keys present in all 26 locales.
5. `src/bindings.ts` contains the new command (regen verified in-diff).
6. Linux/Windows: probe compiles (CI build tiers); untestable-in-CI tiers
   fail open — AC1 covers the logic.

**Test approach**: `cd src-tauri && cargo test memory` plus the full
`cargo test`; `bun run check:translations`. Manual on the target machine
(spec AC4/AC5, batched with T6): idle machine + guard ON, rank-1
(~697 MB) loads with no refusal and the probe reads ≥ 8 GiB free
(undercount guard — unit tests cannot catch this); a multi-GB F16 quant
under induced pressure → toast, no load attempt, current model keeps
transcribing; toggle off → load proceeds.

---

## T6 — `tray-model-ram-status` (M) — spec F4

Display-only: the tray model submenu label and tooltip show the **resident**
model and its approximate footprint (`Parakeet Unified EN 0.6B — 697 MB`
measured / `… — ~697 MB` estimate), fixing the selected-vs-resident
divergence. Unload-now already ships upstream (`tray.rs:549-556` +
`lib.rs:303-311`, both verified this session).

**Files (exact)**
- `src-tauri/src/tray.rs` —
  - `MenuInputs` (struct at `:57`, fields `:58-66`; `#[derive(Clone, Debug,
    PartialEq, Eq)]` at `:56` — grep-verified this session; the derive's
    `PartialEq` is what makes the diffing applier rebuild only on change)
    gains `resident_model: Option<(String, String)>` (id + display name) and
    `model_ram: Option<String>` (pre-formatted).
  - **Extract the model-label resolution into a pure fn** (work item, not
    optional): today the label derivation — prefer `resident_model`, fall
    back to the `selected_model` lookup, else `strings.model` — is inline
    in `build_menu(app: &AppHandle, …)` at `:533-540` (verified), which
    constructs real tauri `Menu`/`Submenu` objects and **cannot run under
    `cargo test` without an app**. The extraction (pure fn taking the
    `MenuInputs` fields + the fallback string) is what makes acceptance 2
    testable at all; `build_menu` then calls it.
  - Pure formatting fn: measured vs `~`-estimate vs absent (omit the
    segment). RSS read fails → `~`-estimate; no estimate → omit; menu build
    never fails on measurement errors. Refresh rides the existing
    `model-state-changed` listener (`lib.rs:355-359`, verified) — no
    polling (C2).
  - **Populate the two new fields in `compute_desired`** — `MenuInputs` is
    constructed in exactly one place: `tray.rs::compute_desired` (fn at
    `:312`, `MenuInputs` struct literal at `:329`; `lib.rs` never builds it
    — it only calls `tray::update_tray_menu`). `compute_desired` already
    reads TranscriptionManager state there (`is_model_loaded()`,
    `tray.rs:316` — verified this session), so it is the natural place to
    source the resident model + footprint: **resident id via the existing
    `get_current_model()`** (`transcription.rs:805-808`, verified this
    session — an existing public wrapper over the private
    `current_model_id`; adding a duplicate accessor is a defect, not a
    work item), display name by mapping the id through the
    `downloaded_models` list `compute_desired` already builds
    (`tray.rs:318-328`), footprint via T5's resident-footprint plumbing
    (worker RSS from the supervisor pid accessor + `rss_bytes_for_pid`
    for TranscribeCpp; `size_mb` estimate for in-process ONNX,
    `transcription.rs:183-194`).
    No wire-protocol change (`LoadedInfo`, `protocol.rs:144-156`, has no
    memory field and v1 does not add one). The existing `inputs(busy)`
    test helper (`tray.rs:688`, verified this session) is extended for
    the inequality tests in acceptance 1.
- Zero new `tray.*` keys: the RAM segment and model names are pre-formatted
  Rust-side; tray strings are build-time codegen from the locale "tray"
  section (`tray_i18n.rs:4-13` per spec). If an implementer adds a key
  anyway, it lands in all 26 locales (C3).

**Acceptance (spec F4 AC1–AC3; AC4 manual)**
1. Unit tests: formatting fn (measured / estimate / absent) and
   `MenuInputs` inequality on `model_ram`/`resident_model` change (drives a
   rebuild).
2. Unit test of the **extracted pure label-resolution fn**: prefers
   `resident_model` over `selected_model` (the failed-switch and
   pending-unload states that produce the divergence), falls back to
   `selected_model`, then to the localized fallback.
3. `bun run check:translations` reports no key drift (zero new keys is the
   expected outcome).
4. `cd src-tauri && cargo test` green.
5. Manual, target machine (spec F4 AC4 + F2 AC3, final pass): load rank-1 →
   label within ±25% of Activity Monitor's worker RSS; unload → segment
   disappears and the "Unload model" item disables after T4's 2-minute
   default.

**Test approach**: `cd src-tauri && cargo test tray`; `bun run
check:translations`; the manual ±25% comparison on the M4 Pro.

---

## Manual-pass batching (who owns which spec AC)

- **M4 Pro, after T6 (single scripted pass)**: spec F1 AC3 (release build,
  network blocked — T2 AC8a), spec F1 AC7 (installed-over-Handy upgrade —
  T3 AC4), spec F3 AC4/AC5 (probe ≥8 GiB idle; pressure refusal; toggle
  off — T5), spec F4 AC4 + F2 AC3 (tray ±25%; 2-minute unload default —
  T6).
- **Windows box, if available (T2 AC8b)**: spec F1 AC5's portable tail
  (fresh VoxBar portable redirects `Data/` + `HF_HOME`; old Handy portable
  still detects). If unavailable: recorded not-run; the extended portable
  unit tests + `installer.nsi:600` template inspection are the automated
  coverage.

## Global risks

1. **Tauri data-dir derivation** — resolved by construction in T3: the
   migration resolves both dirs through `portable::app_data_dir(app)`
   inside `.setup()` (`lib.rs:893`), the same code the app itself uses, so
   no hand-rolled identifier-vs-productName derivation is attempted and the
   original unknown (spec §F1) never has to be answered. Residual: the
   installed-over-Handy outcome is still pinned only by T3's manual AC —
   if Tauri resolves old and new dirs identically the migration no-ops
   safely, but a wrong legacy-dir guess means re-onboarding + model
   re-download for the operator.
2. **T2 breadth**: 26 locales + 5 workflow files + flake/nix modules +
   NSIS + two icon sets in one commit; any missed surface breaks the first
   post-rename CI run — or worse, breaks **silently** where CI has no
   coverage: `nix build .#default`, the `programs.*`/`services.*` module
   options, and `nix/hm-module.nix:34`'s `ExecStart` are never eval'd by
   nix-check.yml, and the Windows `handy.exe` audit (`build.yml:835/:842`)
   only fails on a Windows runner. Mitigation: the classification greps —
   including the dedicated `grep -rn handy flake.nix nix/` and
   build.yml zero-unclassified passes (T2 AC2) — are mandatory in the PR
   body, plus local `nix eval`/`nix flake check` where nix is available.
3. **The compile-uncatchable rename**: `src-tauri/src/audio_toolkit/bin/cli.rs:4`
   sits in the commented-out `[[bin]]` target (`Cargo.toml:22-25`) — only
   the `grep handy_app_lib` check (T2 AC5) catches it.
4. **T1's cap is verified indirectly**: `cargo config get` is nightly-only
   (verified this session on cargo 1.99.0 stable), so the jobs cap is proven
   by parse-success + rustc process-count sampling, not a first-class cargo
   command.
5. **Forecast fidelity**: `size_mb` is a disk-size proxy, not resident RAM
   (research-models §8) — the gate can false-positive/negative. The macOS
   probe undercount failure mode is invisible to unit tests (AC3's
   `0 < n <= hw.memsize` holds under any undercount); only the manual ≥8 GiB
   idle reading (T5, spec F3 AC4) catches it.
6. **Cross-process RSS reads can fail** (permissions/API differences per
   OS): both consumers (gate credit, tray segment) must degrade to the
   estimate / omit — never fail the load path or the menu build.
7. **`bindings.ts` regeneration discipline** (C6): the T5 command is
   invisible to the frontend until a debug build regenerates
   `src/bindings.ts`; forgetting it fails only at runtime.
8. **Translation gate is parity-only**: `check:translations` (green
   baseline confirmed this session) catches key drift, not wrong values —
   the locale sweep must rewrite values in place without adding/renaming
   keys; the embedded `HANDY_DISABLE_UPDATER` case-insensitive survivor is
   exempt; and the new attribution line stays OUT of the locale files
   (eslint-disabled literal) so the exact-case scan remains zero.
9. **SMAppService orphan on the target upgrade path**: a Handy 0.9.8 →
   VoxBar upgrade on macOS 13+ leaves the old bundle's login-item
   registration in place and the new app cannot unregister it
   (SMAppService manages only its own bundle). Documented in T3 (log +
   release notes); the AC asserts documented-orphaned, not code-removed —
   an implementer who promises removal will fail the manual AC late.
10. **Manual ACs cluster on the target machine** (and one on Windows):
    not CI-able; batched per the section above to avoid repeated release
    builds on the memory-constrained machine. The Windows portable check
    may end recorded-as-not-run — that is an honest outcome, not a blocker;
    automated coverage stands.
11. **Updater disablement leaves inert UI**: the tray "Check for updates"
    item and Settings toggle stay visible-and-inert by design (spec F1);
    the no-dialog/no-crash behavior rests on `UpdateChecker.tsx:108-109`
    swallowing the failure — re-verified visually in the release manual
    pass, since any future refactor of that catch could surface a dialog.
12. **Memory pressure during development itself**: T2's lib rename forces a
    full rebuild; T1 must land first or that rebuild runs uncapped (the
    OOM history in the operator's environment notes).
13. **Planning did not compile or test anything**: `cargo check`, `cargo
    test`, and all builds remain unrun by this plan (only
    `bun run check:translations` and the locale scan were executed, both
    green/matching). Every compile/test acceptance above is the
    implementing task's own gate.
