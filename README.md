# VoxBar

VoxBar is offline voice dictation for your Mac. Hold a key, speak, and the text
lands at your cursor. There is no cloud service, no account, and no telemetry:
audio is transcribed by models that run on your machine and the result is
pasted directly into whatever app has focus.

It is a fork of the [Handy](https://github.com/cjpais/Handy) project, reworked
for my own daily use on macOS.

## Features

- Push-to-talk dictation: hold the shortcut while you speak, release to paste.
- Live overlay preview of the text as you speak, supported by the streaming
  models.
- Model choice: pick from the built-in list, or paste a Hugging Face repo id
  to add a model yourself.
- Memory-pressure guard: if the selected model does not fit in free RAM,
  VoxBar can automatically fall back to the best smaller model you already
  downloaded instead of failing the dictation.
- The tray menu shows which model is resident and how much RAM it uses.
- Unload timers to free RAM after idle, including a custom duration.
- Command mode: a second, assignable trigger that turns one dictation into a
  command (for example, asking an assistant to run something).
- Spoken punctuation ("comma", "period") and spoken deletion passes ("scratch
  that", "delete everything").
- Assignable delete-last-word and undo hotkeys to fix dictation mistakes
  without touching the mouse.
- Accent color palette and theme selection.
- Dictation history with a per-entry model badge, so you can see which model
  produced each entry.

## Install

1. Download the release zip for macOS from the
   [releases page](https://github.com/nexiouscaliver/voxbar/releases).
2. Unzip it and drag VoxBar.app to Applications.
3. The app is not signed with a developer certificate. The first launch needs
   a right-click, then "Open", then confirm in the dialog that appears.
4. Grant Accessibility and Microphone access when prompted.

One macOS quirk worth knowing: after a rebuild or an app update, macOS may
treat the app as new and dictation shortcuts can silently stop firing even
though Accessibility still shows as enabled. If that happens, quit VoxBar, run
`tccutil reset Accessibility com.voxbar.app` in a terminal, and grant
Accessibility again on the next launch. Microphone and other permissions are
not affected.

## Build from source

You need:

- Rust (latest stable, from [rustup](https://rustup.rs/))
- [Bun](https://bun.sh/)
- Xcode Command Line Tools (`xcode-select --install`)

Then:

```bash
bun install
bun run tauri dev     # development build with hot reload
bun run tauri build   # release build
```

The finished app bundle ends up in `src-tauri/target/release/bundle/`.

See [BUILD.md](BUILD.md) for platform-specific notes, including Intel Mac and
Windows and Linux requirements.

## Where your data lives

- App settings, history, and downloaded models:
  `~/Library/Application Support/com.voxbar.app/`
- Models pulled from Hugging Face also use the shared Hugging Face cache at
  `~/.cache/huggingface/hub/`, so a model downloaded by another tool is picked
  up without a second copy.

## Platform status

- macOS: this is what I use every day; it is the tested platform.
- Windows and Linux: the upstream Handy project supports both and its CI
  builds them, and this fork keeps that code, but I have not run this fork on
  either platform. Treat them as untested.

## Credit and license

VoxBar is authored by Shahil Kadia. It is built on the
[Handy](https://github.com/cjpais/Handy) project by CJ Pais and its
contributors, and would not exist without their work.

Both the fork and the upstream project are MIT licensed. See
[LICENSE](LICENSE) for the full text, including the preserved upstream
notice.
