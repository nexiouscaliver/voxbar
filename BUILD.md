# Build Instructions

This guide covers how to set up the development environment and build Handy from source across different platforms.

## Prerequisites

### All Platforms

- [Rust](https://rustup.rs/) (latest stable)
- [Bun](https://bun.sh/) package manager
- [Tauri Prerequisites](https://tauri.app/start/prerequisites/)

### Platform-Specific Requirements

#### macOS

- Xcode Command Line Tools
- Install with: `xcode-select --install`

##### Intel Mac (x86_64)

Prebuilt ONNX Runtime binaries are not available for Intel Macs. Install ONNX Runtime via Homebrew and link dynamically:

```bash
brew install onnxruntime
ORT_LIB_LOCATION=$(brew --prefix onnxruntime)/lib ORT_PREFER_DYNAMIC_LINK=1 bun run tauri dev
```

The same environment variables apply for production builds:

```bash
ORT_LIB_LOCATION=$(brew --prefix onnxruntime)/lib ORT_PREFER_DYNAMIC_LINK=1 bun run tauri build
```

#### Windows

- Microsoft C++ Build Tools: Visual Studio 2019/2022 with C++ development
  tools, or Visual Studio Build Tools 2019/2022
- [CMake](https://cmake.org/download/) (must be on `PATH`):

  ```powershell
  winget install Kitware.CMake
  ```

- [Vulkan SDK](https://vulkan.lunarg.com/sdk/home) from LunarG - required to
  build the Vulkan GPU backend (`vulkan-shaders-gen` needs the SDK's headers
  and `glslc`):

  ```powershell
  winget install KhronosGroup.VulkanSDK
  ```

  Open a new terminal afterward so `VULKAN_SDK` is set.

> [!NOTE]
> Windows' 260-character path limit used to break the native Vulkan build in
> most checkouts. Since `transcribe-cpp` 0.1.3 the build works around it
> automatically (it compiles through a short NTFS junction - no admin rights
> or setup needed), so a normal checkout just builds. If you still hit
> path-limit errors, see
> [Windows build fails with path-limit errors](#windows-build-fails-with-path-limit-errors-msb3491--ftk1011--msb6003)
> in Troubleshooting.

#### Linux

- Build essentials
- ALSA development libraries
- Install with:

  ```bash
  # Ubuntu/Debian
  sudo apt update
  sudo apt install build-essential clang libclang-dev libevdev-dev libasound2-dev pkg-config libssl-dev libvulkan-dev vulkan-tools glslc spirv-headers glslang-tools libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libgtk-layer-shell0 libgtk-layer-shell-dev patchelf cmake

  # Fedora/RHEL
  sudo dnf groupinstall "Development Tools"
  sudo dnf install alsa-lib-devel pkgconf openssl-devel vulkan-devel glslc \
    clang clang-devel libevdev-devel \
    spirv-headers-devel spirv-tools-devel glslang \
    gtk3-devel webkit2gtk4.1-devel libappindicator-gtk3-devel librsvg2-devel \
    gtk-layer-shell gtk-layer-shell-devel \
    cmake

  # Arch Linux
  sudo pacman -S base-devel clang libevdev shaderc spirv-headers glslang alsa-lib pkgconf openssl vulkan-devel \
    gtk3 webkit2gtk-4.1 libappindicator-gtk3 librsvg gtk-layer-shell \
    cmake
  ```

## Setup Instructions

### 1. Clone the Repository

```bash
git clone git@github.com:nexiouscaliver/voxbar.git
cd voxbar
```

### 2. Install Dependencies

```bash
bun install
```

Note: `bun install` also regenerates `src-tauri/src/companion/strings_gen.rs`,
the embedded strings for the companion web client (phones as microphones).
Those strings come from the `companion.*` keys of every locale's
`translation.json`; if you edit them, regenerate with:

```bash
bun run gen:companion-strings
```

The generator fails when any locale is missing a key, and
`bun run check:translations` checks the same completeness for the whole
locale files.

### 3. Start Dev Server

```bash
bun tauri dev
```

### 4. Build for Production

```bash
bun run tauri build
```

This compiles a release binary and generates the bundles selected by the platform overlay configs, which Tauri merges over `src-tauri/tauri.conf.json` automatically:

- **Linux** (`src-tauri/tauri.linux.conf.json`): `deb`, `rpm`, and `AppImage` under `src-tauri/target/release/bundle/`. The deb/rpm dependency lists and the AppImage layout live in the base config's `bundle.linux` block.
- **Windows** (`src-tauri/tauri.windows.conf.json`): an `msi` under `src-tauri/target/release/bundle/msi/` (plus the Windows resource mapping in the same overlay).
- **macOS** (base config, `bundle.targets: ["app"]`): the `VoxBar.app` bundle under `src-tauri/target/release/bundle/macos/`, no dmg. Release artifacts are produced by `scripts/release-macos.sh` (see below).

To build just one format on Linux (for example when the AppImage step fails on a rolling-release distro, see the linuxdeploy note in Troubleshooting):

```bash
bun run tauri build -- --bundles deb
```

## Linux Install (from source)

The raw binary (`src-tauri/target/release/voxbar`) cannot run standalone - it needs Tauri resource files (tray icons, sounds, VAD model) to be co-located at the expected path.

**Install from the deb bundle** (works on any Linux distro):

```bash
cd /tmp
ar x /path/to/Handy/src-tauri/target/release/bundle/deb/Handy_*_amd64.deb data.tar.gz
tar xzf data.tar.gz
sudo cp usr/bin/voxbar /usr/bin/
sudo cp -a usr/lib/. /usr/lib/
sudo cp -r usr/share/icons/hicolor/* /usr/share/icons/hicolor/
sudo cp usr/share/applications/VoxBar.desktop /usr/share/applications/
```

The runtime libraries live in the app-private `/usr/lib/VoxBar/` (on the binary's rpath), so no `ldconfig` step is needed.

After subsequent rebuilds, copy the binary and any refreshed runtime libraries:

```bash
sudo cp src-tauri/target/release/voxbar /usr/bin/
sudo mkdir -p /usr/lib/VoxBar
sudo cp -a src-tauri/transcribe-libs/. /usr/lib/VoxBar/
```

Resources only need re-copying if they change upstream (new icons, sounds, models, etc.).

## Releasing (macOS)

Releases are built and signed on the release machine by `scripts/release-macos.sh`. Publishing the GitHub release (tag + uploads) stays a manual, operator-only step.

### One-time: updater signing key

The in-app updater verifies downloaded updates with a minisign (ed25519) keypair. Generate it once on the release machine:

```bash
mkdir -p ~/.voxbar/updater-keys && chmod 700 ~/.voxbar/updater-keys
bunx tauri signer generate -w ~/.voxbar/updater-keys/voxbar.key --password "" --force
chmod 600 ~/.voxbar/updater-keys/voxbar.key ~/.voxbar/updater-keys/voxbar.key.pub
```

The public key's content is embedded in `src-tauri/tauri.conf.json` under `plugins.updater.pubkey`. Custody rules:

- The private key is never committed, never copied into CI, and never leaves the release machine. Only a pointer to its location belongs in notes.
- Back it up offline before the first updater-enabled release ships. If the key is lost, every installed copy can never be updated again: the updater refuses unsigned manifests and signature verification cannot be disabled.

### The script

From the repo root:

```bash
nice -n 15 bash scripts/release-macos.sh
```

It reads the version from `src-tauri/tauri.conf.json` (failing unless `package.json` and `src-tauri/Cargo.toml` agree), checks the key exists with mode 600, builds with `bunx tauri build`, then in a fixed order: re-signs `VoxBar.app` with the stable designated requirement (`designated => identifier "com.voxbar.app"`), zips the app for first installs (`ditto -c -k --keepParent`), tars it for the updater (the `.app` directory must be the archive root because the updater strips the first path component when extracting), signs the tar.gz, and writes `latest.json` with the signature content and both `darwin-aarch64-app` and `darwin-aarch64` platform entries. Artifacts land in `voxbar-build-docs/v<version>/release-assets/` (overridable as the script's first argument).

The order is load-bearing: the build deliberately does not set `createUpdaterArtifacts`, because the designated-requirement re-sign must happen between the build and the tar, and any artifact produced before the re-sign would verify fine but ship the wrong (pre-re-sign) app. Never hand-publish artifacts that skipped a step.

### What each artifact is for

- `VoxBar-<version>-macOS.zip`: first install. Unzip, drag to `/Applications`. The browser download still gets the one-time Gatekeeper treatment (see the README install section).
- `VoxBar.app.tar.gz` + `.sig`: the updater payload and its minisign signature. Installed in-app from 1.1.0 on, no Gatekeeper prompt (the updater downloads without the quarantine flag and swaps the app bundle in place).
- `latest.json`: the manifest the updater fetches (`releases/latest/download/latest.json`). Its `version` must be greater than the installed one or `check()` reports no update.

The identifier-only designated requirement is a deliberate tradeoff for an unnotarized open-source app: it keeps the macOS code identity stable so Accessibility grants survive updates, but any binary claiming the `com.voxbar.app` identifier satisfies it. The stronger variant also pins a certificate leaf, which requires a self-signed code-signing certificate on the release machine.

## Releasing (Linux)

Linux installers are built by `scripts/release-linux.sh` on a Linux machine (there is nothing to sign: the in-app updater ships macOS artifacts only, so Linux users install from the release files):

```bash
nice -n 15 bash scripts/release-linux.sh            # deb + rpm + appimage
nice -n 15 bash scripts/release-linux.sh deb appimage  # or a subset
```

The script reads the version from `src-tauri/tauri.conf.json` (failing unless `package.json` and `src-tauri/Cargo.toml` agree), builds with `bunx tauri build` (the `tauri.linux.conf.json` overlay selects the deb/rpm/AppImage targets), prints sha256 checksums for every artifact, and copies them to
`voxbar-build-docs/v<version>/release-assets-linux/` (overridable as the last argument). `NO_STRIP=true` is passed through to the build for the linuxdeploy old-strip problem described in Troubleshooting.

Publishing the GitHub release (tag + uploads) is the same manual, operator-only step as on macOS. Do not add the Linux files to `latest.json`: the updater manifest is macOS-only until signed Linux update artifacts exist (see `src/components/update-checker/updaterPlatform.ts`).

## Troubleshooting

### macOS Accessibility remains enabled after a local rebuild

Local builds use the ad-hoc `signingIdentity: "-"`. A rebuild can have a new macOS code
identity while the old **System Settings > Privacy & Security > Accessibility** entry
remains visibly enabled, leaving Handy on `Waiting...`.

After installing the final bundle at `/Applications/VoxBar.app`, quit VoxBar, clear only its
stale Accessibility record, then reopen it:

```bash
osascript -e 'tell application id "com.voxbar.app" to quit' || true
tccutil reset Accessibility com.voxbar.app
open /Applications/VoxBar.app
```

Grant Accessibility again when prompted. This does not reset Microphone or other TCC
services, and official releases normally do not need it.

For optional diagnosis, compare the designated requirements of the previous and rebuilt
bundles:

```bash
codesign -dr - /path/to/previous/Handy.app 2>&1
codesign -dr - /Applications/VoxBar.app 2>&1
```

An ad-hoc requirement contains a `cdhash`; a changed requirement confirms the rebuild is
not covered by the old grant. The reset procedure does not require this check.

See [issue #1618](https://github.com/cjpais/Handy/issues/1618) for the related onboarding
and stale-permission report.

### AppImage build fails on Arch / rolling-release distros

`linuxdeploy` bundles its own `strip` binary which is too old to process system libraries built with newer toolchains on rolling-release distros (Arch, CachyOS, Manjaro, EndeavourOS).

The error from Tauri:

```
Bundling VoxBar_*_amd64.AppImage
failed to bundle project `failed to run linuxdeploy`
```

Tauri swallows the real linuxdeploy error. To see it, run linuxdeploy manually:

```bash
cd src-tauri/target/release/bundle/appimage
~/.cache/tauri/linuxdeploy-x86_64.AppImage --appimage-extract-and-run \
  --appdir VoxBar.AppDir --plugin gtk --output appimage
```

**Workaround:** The binary, deb, and rpm bundles all build fine - only the AppImage step fails. Either skip the AppImage target:

```bash
bun run tauri build -- --bundles deb
```

or tell linuxdeploy not to strip at all (it then leaves the binaries' symbols in place):

```bash
NO_STRIP=true bun run tauri build
```

`scripts/release-linux.sh` passes `NO_STRIP` through to the build, so `NO_STRIP=true bash scripts/release-linux.sh deb` covers this case in one step.

Then install using the deb extraction method above.

### Windows build fails with path-limit errors (`MSB3491` / `FTK1011` / `MSB6003`)

On Windows the native build can fail partway through `transcribe-cpp-sys` with
any of these (all the same root cause):

```
error MSB3491: Could not write lines to file "...VCTargetsPath.tlog\VCTargetsPath.lastbuildstate".
Path: ... exceeds the OS max path limit. The fully qualified file name must be less than 260 characters.
```

```
FileTracker : error FTK1011: could not create the new file tracking log file:
...\vulkan-shaders-gen-build\...\cmTC_xxxxx.tlog\link.write.1.tlog.
The system cannot find the path specified.
```

```
error MSB6003: The specified task executable "CL.exe" could not be run.
System.IO.DirectoryNotFoundException: Could not find a part of the path ...
```

This is **not** a code or toolchain problem - it's Windows' legacy 260-character
path limit (`MAX_PATH`), overflowed by the Vulkan shader generator's nested
CMake build tree on top of Cargo's already-deep
`target\release\build\<crate>-<hash>\out\build\...` directory.

Since `transcribe-cpp` 0.1.3 this is mitigated automatically: the native build
compiles through a short NTFS junction under `%LOCALAPPDATA%\tcs` (created
without admin rights), so a normal checkout builds with no setup. Enabling
Windows long paths does **not** reliably help here - MSBuild's native
`FileTracker` (`tracker.exe`) ignores the long-paths flag - which is why the
junction, not the registry flag, is the fix.

If you still see the errors above, junction creation was likely blocked
(filesystem or corporate policy) - the failing build's log then contains a
`transcribe-cpp-sys: could not create short build junction ...` warning - or
your checkout is deep enough to overflow even the shortened layout. Work
around either case with a short Cargo target directory:

```powershell
# Per-shell:
$env:CARGO_TARGET_DIR = "C:\h"

# Or persist it for all future terminals (note: redirects ALL your
# Rust projects' build output, not just Handy):
[Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', 'C:\h', 'User')
```

Artifacts then land in `C:\h\release\...` instead of the repo's
`src-tauri\target\`. Open a **new terminal** if you persisted the variable -
it is only picked up by freshly started processes. Then `bun run tauri dev`
and `bun run tauri build` work normally.

### Windows `tauri build` fails at bundling with `program not found`

If the build compiles all the way to `Built application at: ...\voxbar.exe` and
then fails with:

```
Signing C:\...\voxbar.exe with a custom signing command
failed to bundle project `program not found`
```

that's the code-signing step: `tauri.conf.json` configures a custom
`signCommand` (`trusted-signing-cli`, Azure Trusted Signing) that only exists
in the release CI environment. Local development doesn't need it:

```powershell
# Development (no bundling/signing at all):
bun run tauri dev

# Or compile a release binary without the installer/signing step:
bun run tauri build --no-bundle
```
