# Research: How handy-dictation (upstream cjpais/Handy) builds and verifies itself

Area: build system, test/CI verification, release bundling, signing, identity config.
Scope: the `handy-dictation/` checkout inside mission-control. Git remote is
`https://github.com/cjpais/Handy.git`, HEAD at research time was `c8262ab`
"isolate transcribe.cpp backend in own worker (#2208)" (verified via
`git -C handy-dictation log --oneline -3`). App version everywhere: **0.9.8**.

All path references below are relative to `handy-dictation/` unless absolute.

---

## 1. Toolchain and package management

- **Bun, not npm/yarn/pnpm.** The repo has `bun.lock` and no `package-lock.json`
  (which is explicitly gitignored — `.gitignore:12`). Every script and doc uses
  `bun …` (`package.json:6-23`, `AGENTS.md:14-41`, `BUILD.md:99-115`). CI uses
  `oven-sh/setup-bun@v2` and `bun install --frozen-lockfile`
  (`.github/workflows/code-quality.yml:42-47`, `playwright.yml:23-27`).
- Local bun available during this research: `bun 1.3.10`, `cargo 1.99.0`.
- **No Justfile / Makefile** exists (checked top two levels with `find`; only
  `package.json`, `flake.nix`, CI YAML drive builds). Nix (`flake.nix`) is a
  parallel Linux packaging path, not the primary one.
- **Rust**: latest stable via rustup locally; CI pins
  `dtolnay/rust-toolchain@stable` (`build.yml:104-108`).

## 2. Repo / workspace layout

- Single Rust crate, **not a Cargo workspace**: `src-tauri/Cargo.toml:1-20`
  defines package `handy` v0.9.8, lib name `handy_app_lib`
  (`crate-type = ["staticlib", "cdylib", "rlib"]`), `default-run = "handy"`.
  Verified by running
  `cargo metadata --manifest-path src-tauri/Cargo.toml --no-deps --offline`:
  targets `["handy_app_lib", "handy", "build-script-build"]`,
  `workspace_root` = `…/handy-dictation/src-tauri` (standalone crate root, no
  `[workspace]` section anywhere — `find` shows only one `Cargo.toml`).
- `src-tauri/Cargo.lock` is committed.
- Release profile is maximal: `lto = true`, `codegen-units = 1`, `strip = true`
  (`src-tauri/Cargo.toml:201-205`).
- Version lives in **three places that must stay in sync**:
  `package.json:4` (`"version": "0.9.8"`), `src-tauri/Cargo.toml:3`, and
  `src-tauri/tauri.conf.json:4`. CI and the release flow read the version from
  `tauri.conf.json` (`build.yml:81-87`, `release.yml:17-23`); Nix reads it from
  `Cargo.toml` (`flake.nix:30-31`).

## 3. Developer commands (exact, from package.json + AGENTS.md/BUILD.md)

`package.json:6-23` scripts:

| Script | Command | What it does |
| --- | --- | --- |
| `dev` | `vite` | frontend-only dev server |
| `build` | `tsc && vite build` | typecheck + production frontend bundle into `dist/` |
| `preview` | `vite preview` | serve built frontend |
| `tauri` | `tauri` | tauri CLI passthrough |
| `lint` | `eslint src` | ESLint (i18next literal-string rule) |
| `lint:fix` | `eslint src --fix` | |
| `format` | `prettier --write . && cd src-tauri && cargo fmt` | both languages |
| `format:check` | `prettier --check . && cd src-tauri && cargo fmt -- --check` | CI check |
| `test:keyboard` | `bun src/lib/utils/keyboard.test.ts` | plain-bun unit test, no deps |
| `test:playwright` | `playwright test` | e2e smoke (2 tests) |
| `test:playwright:ui` | `playwright test --ui` | |
| `check:translations` | `bun scripts/check-translations.ts` | locale key parity vs `en` |
| `check:model-languages` | `bun scripts/check-model-language-coverage.ts` | catalog↔frontend language intents |
| `postinstall` | `bun scripts/check-nix-deps.ts` | regenerates `.nix/bun.nix` when `bun.lock` changes (no-op on Windows, warns-not-fails if bun2nix missing — `scripts/check-nix-deps.ts:33-77`) |

Documented top-level flow (`AGENTS.md:14-41`, `BUILD.md:99-115`):

```bash
bun install
bun run tauri dev     # dev app (macOS cmake fallback: CMAKE_POLICY_VERSION_MINIMUM=3.5)
bun run tauri build   # production bundles (deb/rpm/AppImage, dmg, msi/nsis)
bun run tauri build --no-bundle   # release binary only, skips Windows signing step (BUILD.md:276-281)
bun run tauri build -- --bundles deb  # subset of Linux bundles (BUILD.md:203)
```

- **There is no separate `cargo test` npm script**; Rust tests are run directly
  (`cd src-tauri && cargo test` — exactly what CI does at `test.yml:36-37`).
- `cargo clippy` is **recommended locally** (`CONTRIBUTING.md:246`,
  `AGENTS.md:159-161`) but is **NOT run in any CI workflow** (grep over
  `.github/workflows/` finds no clippy — verified, no match).
- Dev-only model setup: download `silero_vad_v4.onnx` into
  `src-tauri/resources/models/` (`AGENTS.md:43-48`,
  `CONTRIBUTING.md:53-58`). In this clone the file is **already git-tracked**
  (`git ls-files src-tauri/resources/models/` shows `silero_vad_v4.onnx` and
  `gigaam_vocab.txt`), so no download is needed here.

## 4. Frontend toolchain (vite/ts/eslint/prettier)

- Vite 6 + `@vitejs/plugin-react` + `@tailwindcss/vite` (Tailwind v4)
  (`package.json:52-67`, `vite.config.ts:9-11`).
- **Two entry points**: `index.html` (main) and `src/overlay/index.html`
  (recording overlay) via `build.rollupOptions.input`
  (`vite.config.ts:21-28`).
- Dev server: port **1420**, `strictPort: true`, `TAURI_DEV_HOST` env for
  non-localhost HMR on port 1421; watches ignore `**/src-tauri/**`
  (`vite.config.ts:34-50`). `tauri.conf.json:6-11` wires
  `beforeDevCommand: bun run dev`, `devUrl: http://localhost:1420`,
  `beforeBuildCommand: bun run build`, `frontendDist: ../dist`.
- Path aliases `@` → `./src` and `@/bindings` → `./src/bindings.ts` in both
  `vite.config.ts:13-18` and `tsconfig.json` `paths`.
- TypeScript ~5.6, `strict`, `noEmit`, bundler moduleResolution
  (`tsconfig.json`). The `build` script's `tsc` step is the only typecheck —
  there is no separate `typecheck` script and CI has no dedicated tsc job;
  type errors surface via `bun run build` inside tauri builds and via ESLint's
  TS parser.
- ESLint 9 flat config, single rule set on `src/**/*.{ts,tsx}`:
  `i18next/no-literal-string` (markupOnly, common attrs ignored)
  (`eslint.config.js`). Prettier config is just `{"endOfLine":"lf"}`
  (`.prettierrc`); `rustfmt.toml` is `edition = "2021"`.
- `src/bindings.ts` (Tauri command/event TS bindings) is **auto-generated by
  tauri-specta on debug builds only** (`src-tauri/src/lib.rs:778-784`,
  `#[cfg(debug_assertions)]`, exports to `../src/bindings.ts`). It is committed
  (`git ls-files src/bindings.ts`), so release builds don't regenerate it.

## 5. Rust build-time codegen and staging (`src-tauri/build.rs`)

`build.rs:1-38` runs, in order:

1. **Apple Intelligence Swift bridge** (macOS aarch64 only,
   `build.rs:2-3,384-556`): compiles `swift/apple_intelligence.swift` (or the
   stub) with `swiftc -parse-as-library`, `libtool -static`, links
   `Foundation` (+ weak-links `FoundationModels`). Auto-detects
   Command-Line-Tools-only toolchains and falls back to stubs
   (`build.rs:425-444`); `HANDY_FORCE_AI_STUB=1` forces stubs; `SDKROOT`/`SWIFTC`
   env overrides for non-Xcode toolchains (nix).
2. **Tray translations codegen** (`build.rs:284-362`): reads
   `../src/i18n/locales/*/translation.json` (frontend tree!) and emits
   `tray_translations.rs`. → **The Rust build depends on frontend source files
   being present**; `src-tauri` cannot build from a frontend-less checkout.
3. **Linux rpath** (`build.rs:18-20`): links
   `-Wl,-rpath,$ORIGIN/../lib/Handy:$ORIGIN/../lib` into `handy`.
4. **Stage transcribe-cpp runtime libs** (`build.rs:165-261`): copies
   `libtranscribe.so*`/`libggml*.so*` (or Windows DLLs) from the
   transcribe-cpp-sys install dirs (`DEP_TRANSCRIBE_CPP_RUNTIME_DIR` /
   `MODULE_DIR`) into `src-tauri/transcribe-libs/`, dedup by SONAME. Panics if
   a shared-posture build stages zero libs.
5. **Stage `onnxruntime.dll`** when `ORT_PREFER_DYNAMIC_LINK`+`ORT_LIB_LOCATION`
   set on Windows (`build.rs:113-147`).
6. **Stage VC++ runtime DLLs** when `HANDY_VC_REDIST_DIRS` set (Windows CI):
   msvcp140/vcruntime140/vcomp140 families; **panics** if msvcp140.dll or
   vcruntime140.dll missing (`build.rs:49-103`).
7. `tauri_build::build()`.

`src-tauri/.gitignore` ignores `/target/`, `/gen/schemas`, and
`/transcribe-libs/` (staging dir created by build.rs).

## 6. Tests: what exists, what needs network/models, how CI stays hermetic

### Rust tests (`cargo test` in `src-tauri/`)

- 36 source files contain `#[cfg(test)]`; **311 test functions** total
  (counted via grep `#\[test\]|#\[tokio::test\]`).
- **CI runs plain `cargo test` on ubuntu-24.04**
  (`.github/workflows/test.yml:17-37`) with apt deps
  `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev libasound2-dev
  libssl-dev libgtk-layer-shell-dev libvulkan-dev glslc spirv-headers`
  (`test.yml:22-29`) — those native headers are needed to *compile* the crate
  (Vulkan backend of transcribe-cpp), not to run tests.
- **Model/GPU-dependent tests are `#[ignore]`d**: the engine_supervisor
  end-to-end suite (17 ignored tests,
  `src-tauri/src/engine_supervisor/supervisor.rs:1572-1932`) requires
  `HANDY_TRANSCRIBE_WORKER_EXE`, `HANDY_TEST_MODEL` (a GGUF model path) and
  `HANDY_TEST_WAV`, run manually via
  `cargo test --lib engine_supervisor -- --ignored --nocapture --test-threads=1`
  (instructions in the module doc, `supervisor.rs:1477-1483`). Plain
  `cargo test` skips them, so **CI never needs a transcription model**.
- **Network tests are loopback-only**: the resumable-downloader suite spins up
  `TcpListener::bind("127.0.0.1:0")` servers — "no Tauri, no real network"
  (`src-tauri/src/managers/model/download/tests.rs:84,120,131,365`). The
  unreachable-URL test points at `http://127.0.0.1:9/unreachable`
  (`download/tests.rs:375`).
- The Nix build sets `doCheck = false` precisely because tests would need
  "audio devices, model files, GPU/Vulkan" (`flake.nix:155-157`).

### Frontend tests

- `test:keyboard` — dependency-free bun script using `node:assert`
  (`src/lib/utils/keyboard.test.ts`).
- Playwright: `tests/app.spec.ts` — exactly 2 smoke tests ("dev server
  responds" 200, "page has html structure"). Config
  (`playwright.config.ts`): chromium only, `webServer: bunx vite dev` on
  :1420, `workers: 1` on CI, html reporter. CI installs chromium via
  `bunx playwright install chromium` (`playwright.yml:29-33`). It tests the
  **vite dev server, not the Tauri app** — no Rust build involved.
- Repo-level checks: `check:translations` (every locale has exactly the `en`
  key set — `scripts/check-translations.ts:92-221`) and
  `check:model-languages` (every `catalog.json` model language maps to exactly
  one frontend language intent — `scripts/check-model-language-coverage.ts`).

### Checks I ran in this session (read-only, no network)

- `bun run test:keyboard` → **passed** ("keyboard: all assertions passed",
  exit 0).
- `bun run check:translations` → **passed** ("All 25 languages have complete
  translations!", exit 0).
- `cargo metadata --manifest-path src-tauri/Cargo.toml --no-deps --offline`
  → parsed OK; single package `handy` 0.9.8 (see §2).
- **Not run** (heavy / would compile or download): `cargo test`, `cargo
  build`, `bun run lint`, `bun run format:check`, `playwright test`,
  `bun run tauri build`. `bun install` was not needed for the two bun checks
  above (they don't import node_modules deps).

## 7. CI workflows (`.github/workflows/`, 9 files)

| Workflow | Trigger | What it does |
| --- | --- | --- |
| `test.yml` | push/PR touching `src-tauri/**` | `cargo test` on ubuntu-24.04 (+rust-cache `workspaces: "./src-tauri -> target"`) |
| `code-quality.yml` | push/PR touching `src/**`, `scripts/**`, `package.json`, `bun.lock`, lint/ts configs, catalog.json, workflows | `bun install --frozen-lockfile`; `check:translations`; `check:model-languages`; `test:keyboard`; `lint`; `format:check` |
| `playwright.yml` | PR touching `src/**`, `tests/**`, `package.json`, `bun.lock`, `playwright.config.*` | bun install, `bunx playwright install chromium`, `test:playwright` |
| `build.yml` | `workflow_call` only | **the reusable cross-platform builder** — see below |
| `main-build.yml` | push to main | 7-target matrix, `sign-binaries: true`, uploads 30-day artifacts |
| `release.yml` | manual `workflow_dispatch` | creates a **draft** GitHub release tagged `v{tauri.conf.json version}`, then the same 7-target matrix with `no-cache: true` (release builds never restore the Rust cache so a stale native lib can't be linked — `release.yml:79-82`) |
| `build-test.yml` | manual | full signed matrix, `asset-prefix: handy-test`, uploads artifacts |
| `pr-test-build.yml` | manual, takes `pr_number` | builds `refs/pull/N/merge`, comments artifact links on the PR |
| `nix-check.yml` | push/PR touching nix/bun.lock/src | regenerates `.nix/bun.nix` via `bunx bun2nix` and diffs (fails if stale), `nix eval`, full `nix build .#handy` when nix packaging files changed or on main; pushes to Cachix `handy-computer` |

### The build matrix (identical in main-build/release/build-test/pr-test-build)

- `macos-26` → `--target aarch64-apple-darwin` (macOS 26 runner chosen for the
  Apple Intelligence SDK)
- `macos-latest` → `--target x86_64-apple-darwin`
- `ubuntu-22.04` → `--bundles deb` (x86_64)
- `ubuntu-24.04` → `--bundles appimage,rpm` (x86_64)
- `ubuntu-24.04-arm` → `--bundles appimage,deb,rpm` (aarch64)
- `windows-latest` → x86_64-pc-windows-msvc (default bundles: msi+nsis)
- `windows-11-arm` → `--target aarch64-pc-windows-msvc`

### What `build.yml` adds around `tauri-action@v0`

- Version scraped from `tauri.conf.json` (`build.yml:81-87`); bun + rust
  setup; per-OS apt deps (webkit2gtk pinned `=2.44.0-2` on ubuntu-24.04 x64,
  `build.yml:121-131`); Vulkan SDK installs per platform; `rustup target add`
  for cross targets.
- **ONNX Runtime dynamic linking** (ORT 1.24.2) — downloaded per platform:
  macOS x86_64 from `https://blob.handy.computer/onnxruntime-osx-x86_64-…tgz`
  and bundled as a macOS framework via jq edit of tauri.conf.json
  (`build.yml:361-374`); ubuntu-22.04 x86_64 likewise, shipped as
  `/usr/lib/Handy/libonnxruntime.so.1` (`build.yml:376-399`); Windows x64 from
  Microsoft's GitHub release (`build.yml:413-425`). Env: `ORT_LIB_LOCATION`,
  `ORT_PREFER_DYNAMIC_LINK=1`.
- Windows x64: `vcpkg install spirv-headers:x64-windows` + `CMAKE_PREFIX_PATH`
  (`build.yml:438-447`); Windows ARM64: clang-cl/lld/Ninja config +
  `TRANSCRIBE_CMAKE_ARGS="-DGGML_NATIVE=OFF -DGGML_OPENMP=OFF"`
  (`build.yml:265-300`); VC++ redist dirs located via vswhere →
  `HANDY_VC_REDIST_DIRS` (`build.yml:306-335`); signtool path for ARM64
  (`build.yml:340-359`).
- Linux: pre-build (`bun run build` + `cargo build`) to materialize the
  transcribe-cpp install dir, then `LD_LIBRARY_PATH` + copy to `/usr/local/lib`
  so linuxdeploy can resolve `libtranscribe.so` (`build.yml:457-491`); a
  current `linuxdeploy` is installed into Tauri's tool cache
  (`build.yml:497-513`); `LINUXDEPLOY_EXCLUDED_LIBRARIES=libvulkan.so*;libwayland-client.so*`
  and `LINUXDEPLOY_OUTPUT_VERSION` env (`build.yml:537-540`).
- Build step: `tauri-apps/tauri-action@v0` with `args: <build-args>` and all
  signing env (GITHUB_TOKEN, APPLE_*, AZURE_*, TAURI_SIGNING_PRIVATE_KEY…)
  (`build.yml:515-546`).
- **Post-build audits** (all fail the job):
  - AppImage `X-AppImage-Version` check (`build.yml:551-580`, `718-781`);
  - macOS x86_64 `@rpath` dependency check via `otool -L` (`build.yml:582-601`);
  - Linux deb/rpm/AppImage content audit + `xvfb-run … handy --list-devices`
    smoke with `HANDY_NO_GTK_LAYER_SHELL=1` and Mesa llvmpipe ICD, asserting
    "load_backend: loaded Vulkan backend" in the log (`build.yml:613-781`);
  - Windows: staged-DLL presence in `src-tauri/transcribe-libs`, silent NSIS
    install (`/S /PORTABLE`), MSI administrative install (`msiexec /a`),
    `handy.exe --list-devices` run, and ARM64 CPU-only/GPU-none device
    assertions (`build.yml:795-895`).

## 8. Release bundle, tauri config, signing

`src-tauri/tauri.conf.json` (schema v2):

- `productName: "Handy"`, `version: "0.9.8"`, `identifier: "com.pais.handy"`
  (`tauri.conf.json:3-5`).
- `app.macOSPrivateApi: true`; CSP null; asset protocol enabled with scope
  `**` (`tauri.conf.json:12-24`).
- `bundle`: `active`, `createUpdaterArtifacts: true`, `targets: "all"`,
  `resources: ["resources/**/*"]`, license MIT, **icons**
  `icons/32x32.png, 128x128.png, 128x128@2x.png, icon.icns, icon.ico`
  (`tauri.conf.json:26-38`). The `src-tauri/icons/` dir additionally holds
  Windows Store square logos, `android/` + `ios/` icon sets, `logo.png`.
- macOS: `hardenedRuntime: true`, `minimumSystemVersion: "10.15"`,
  `signingIdentity: "-"` (ad-hoc for local builds; CI overrides via
  `APPLE_SIGNING_IDENTITY` env → tauri-action), entitlements file
  `Entitlements.plist` = microphone + audio-input (`tauri.conf.json:39-45`,
  `src-tauri/Entitlements.plist`). `Info.plist` carries
  `NSMicrophoneUsageDescription`. Apple cert import + notarization identities
  handled in `build.yml:221-243` (`APPLE_CERTIFICATE`, `APPLE_ID`,
  `APPLE_TEAM_ID`… secrets, only when `sign-binaries`).
- Windows: custom `signCommand` invoking `trusted-signing-cli` against Azure
  Trusted Signing (`eus.codesigning.azure.net`, account `CJ-Signing`, profile
  `cjpais-dev`) (`tauri.conf.json:72-78`) — **local builds must use
  `--no-bundle` or `tauri dev` because the CLI only exists in release CI**
  (`BUILD.md:261-281`). NSIS installer uses a custom template
  `nsis/installer.nsi` + `icons/icon.ico` (`tauri.conf.json:74-77`).
  `trusted-signing-cli@0.9.0` is cargo-installed in CI when signing
  (`build.yml:52,166-176`).
- Linux: deb depends `libgtk-layer-shell0, libopenblas0` and maps
  `transcribe-libs` → `/usr/lib/Handy`; rpm same with compression none;
  appimage `bundleMediaFramework: true`, `files: {"/usr/lib":
  "transcribe-libs"}` (`tauri.conf.json:46-71`). `tauri.windows.conf.json`
  (merged for Windows builds) maps `resources` and `transcribe-libs` → `.`
  (install root, beside `Handy.exe`).
- **Updater**: `plugins.updater` with embedded minisign public key and
  endpoint `https://github.com/cjpais/Handy/releases/latest/download/latest.json`
  (`tauri.conf.json:80-87`). Updater artifacts are minisign `.sig` files;
  manual verification recipe with `minisign` in `README.md:238-272`. The
  signing private key comes from `TAURI_SIGNING_PRIVATE_KEY`(+password)
  secrets in CI (`build.yml:529-530`).
- Release flow (`release.yml`): manual dispatch → draft release created from
  tauri.conf.json version with generated notes → all 7 platform builds upload
  assets into it (tauri-action does the upload, `releaseId` input). A release
  is published by a human flipping the draft, not by CI.

## 9. Nix path (secondary)

`flake.nix` builds `packages.<system>.handy` via
`rustPlatform.buildRustPackage` with `cargoRoot = "src-tauri"`,
`tauriBundleType = "deb"`, `allowBuiltinFetchGit` for git deps, bun deps via
`bun2nix.fetchBunDeps` from `.nix/bun.nix`, `doCheck = false`, updater
force-disabled via `HANDY_DISABLE_UPDATER=1` wrapper (`flake.nix:89-186`).
Linux-only (x86_64/aarch64). Dev shell + NixOS/home-manager modules included
(`flake.nix:192-251`). `nix-check.yml` keeps `.nix/bun.nix` in sync with
`bun.lock` (the `postinstall` hook regenerates it locally).

## 10. Seams our features should build on

- `package.json` scripts block (`package.json:6-23`) — add new checks here;
  `code-quality.yml:49-62` is where a new frontend check gets wired into CI
  (it just calls `bun run <script>`).
- `test.yml` is the single place Rust tests enter CI (`cargo test`,
  `test.yml:35-37`); its `paths:` filter (`src-tauri/**`) decides when it
  runs.
- `src-tauri/tauri.conf.json` is the identity seam: productName / identifier /
  version / icons / updater endpoints all live in one file
  (`tauri.conf.json:3-5,32-38,80-87`); `tauri.windows.conf.json` is the
  Windows-only overlay.
- `build.yml` is the one reusable builder every matrix workflow calls; its
  inputs (`sign-binaries`, `no-cache`, `is-debug-build`, `asset-prefix`,
  `upload-artifacts`, `ref`) are the extension points
  (`build.yml:3-49`).
- `scripts/` holds the repo's own check-script pattern (plain `.ts` run by
  bun, exit 1 on failure) — `check-translations.ts` is the template.
- `src/bindings.ts` regenerates automatically on any debug run
  (`lib.rs:778-784`); Rust command surface changes need a `tauri dev` run to
  refresh the committed bindings.
- The `postinstall` hook (`package.json:22`) already runs on every
  `bun install` — a natural place repo-wide invariants get enforced cheaply.

## 11. Risks / gotchas for our build

- **Rust build requires the frontend tree**: build.rs reads
  `../src/i18n/locales/*/translation.json` (`build.rs:290-314`), and tauri's
  `generate_context!` needs `frontendDist` (`../dist`) to exist — CI even runs
  `bun run build` before the Linux pre-build for this reason
  (`build.yml:463-467` comment "Frontend must exist for the app crate to
  compile").
- **Windows local `tauri build` fails at bundling** unless Azure
  trusted-signing-cli is available — use `--no-bundle` locally
  (`BUILD.md:261-281`).
- macOS local builds are ad-hoc signed (`signingIdentity: "-"`), which breaks
  Accessibility grants across rebuilds; documented reset via `tccutil`
  (`BUILD.md:149-179`).
- Intel-mac builds need `ORT_LIB_LOCATION=… ORT_PREFER_DYNAMIC_LINK=1` from
  Homebrew onnxruntime (`BUILD.md:20-33`).
- Version bumps must touch package.json + Cargo.toml + tauri.conf.json
  together; CI trusts only tauri.conf.json for release tagging.
- No clippy gate in CI — regressions there are invisible until a maintainer
  runs it locally.
- Playwright coverage is minimal (dev-server smoke only); the real app is
  never e2e-tested in CI.
- `cargo test` on a machine without the Vulkan/glslc headers will fail to
  *compile* the test binary on Linux (CI installs them at `test.yml:28-29`);
  on macOS the metal feature path needs Xcode CLT at minimum (AI bridge
  auto-stubs under CLT-only, `build.rs:425-444`).
- The engine_supervisor e2e suite (worker crash/hang recovery) is invisible to
  CI — `#[ignore]` + manual env-var harness (`supervisor.rs:1477-1502`).
- App runs `--list-devices` headless smoke in CI under xvfb; a similar local
  smoke needs a display or will fail on Linux (GTK init) — `build.yml:652-668`.
