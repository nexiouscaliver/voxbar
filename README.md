# VoxBar

VoxBar is fast, private dictation for your Mac. Hold a key, speak, and clean
text lands at your cursor, transcribed and polished entirely on your machine.

By default nothing you say leaves the Mac. There is no cloud service, no
account, and no telemetry, and the transcription model runs locally. The app
talks to the network for three things only: downloading models you ask for,
checking for updates if you leave that on, and the optional cloud polish
provider you would have to configure yourself. The result is pasted straight
into whatever app has focus.

While you dictate, a single overlay carries everything: live text, a
waveform, and anything the app needs to tell you. Feedback reaches you there,
in a notification, or as a sound. You do not need a settings window open to
know what happened.

## Features

### Live overlay

Hold your hotkey and start talking. Words appear on the overlay as you speak:
finished words in solid text, the word still forming in lighter text, so you
always know what the model heard. On Parakeet Unified EN 0.6B, the first
featured pick, words show up roughly a third of a second after you say them:
VoxBar starts the stream at a faster setting the model supports (smaller
audio chunks, shorter lookahead) instead of the model's default, which can
leave words landing up to about two seconds late. When you release, the text
is pasted where your cursor is.

- A waveform shows the microphone is picking you up, and an elapsed timer
  shows how long you have been going.
- Fix mistakes without the mouse. Delete a word, by hotkey or by voice, and
  the overlay shows what it removed on a small chip for a moment, struck
  through, so a deletion is always visible.
- A streaming session keeps capturing for 200 ms after you release the key,
  so a quick release does not clip the last word. (Adjustable in Output
  settings.)
- Batch models like the Whisper family do not stream; they show the finished
  text on the overlay for a beat before the paste, so you still get a last
  look.
- The overlay sits at the top or bottom of the screen. You can scroll back
  through what you said while still recording; it follows the newest line
  again when you scroll back down.

### On-device polish pass

Raw transcripts have rough edges. VoxBar can clean them up after
transcription, and you write the rules: prompt templates in Settings, with a
placeholder for the transcript. "Fix grammar and typos, keep my wording" is a
fine first prompt.

Three providers, two of them local:

- A bundled local model: Qwen3 0.6B, quantized, a one-time download of about
  610 MB. It runs through llama.cpp on your Mac.
- Apple Intelligence, on Macs that have it (Apple Silicon, macOS Tahoe 26.0
  or later, enabled in System Settings). This calls the system's on-device
  language model directly.
- Any OpenAI-compatible API. This is the one off-device option, and it is
  labeled as such in Settings.

Turn the polish on and every dictation gets it, or leave it off and use the
dedicated polish hotkey when a transcript deserves it. While it runs, the
overlay switches to a Processing row so the pause is explained. The voice
model and the polish model are not in RAM at the same time: VoxBar unloads
one, runs the other, restores, and the memory gate forecasts the model's
footprint from its 610 MB file size, so the swap only happens when that fits
your free RAM. The local pass gives up after 45 seconds, cloud requests time
out after 60 by default (5 to 600 in Settings), and a press of the cancel
key aborts either. When the polish cannot run (low RAM, transcript too
long, model too slow), VoxBar pastes the raw transcript and tells you why on
the overlay, so a dictation is not lost to the polish step.

### Memory guard and model fallback

VoxBar checks free memory before every model load, and the check understands
macOS memory pressure: reclaimable cache counts when the system is
comfortable and counts less as pressure rises.

If the model you picked will not fit in free RAM, VoxBar does not fail the
dictation. With auto-fallback on (the default), it loads the best smaller
model you already have and names it on the overlay in a neutral chip. A
safety margin setting decides how much spare RAM must remain beyond the
model's size: off by default, with presets from 256 MB to 1536 MB or a
custom number.

The tray menu shows which model is resident and how much RAM it uses,
refreshing live. Unload it on demand, or set an idle timer: after 2 minutes
(the default), 5, 10, or 15 minutes, an hour, immediately, never, or a
custom duration.

### Spoken commands

Out of the box, saying "comma" gives you a comma. Thirty commands ship built
in, in four groups:

- Punctuation (9): period, comma, question mark, exclamation, colon,
  semicolon, dash, new line, new paragraph.
- Symbols (16): at sign, hash, dollar sign, percent, star, ampersand, caret,
  open parenthesis, close parenthesis, open bracket, close bracket, open
  brace, close brace, slash, backslash, pipe.
- Editing (3): delete word, delete line, clear all.
- Control (2): undo, paste.

Command mode is a modifier key you bind yourself: hold it mid-dictation and
everything you say is treated as commands that edit your text directly. The
overlay shows a badge while it is active. Release to go back to dictating.

The command engine is careful with plain speech. "A period of time" and
"five star hotel" stay words, while a command word at the end of a sentence
still converts. In command mode, words the engine does not recognize are
discarded rather than pasted. Voice deletion understands counts ("delete
last three words" deletes three), and terminal punctuation can end every
transcript with a period or question mark automatically.

Built-in phrases cover English, Hindi (Devanagari and romanized spellings)
and Chinese. For any other language, add your own phrases in the command
matrix; they work everywhere commands do.

### Spoken numbers

Say "one eight zero one" and 1801 lands in your editor. Three modes live in
Settings, Advanced, under Spoken Numbers:

- Digits (the default): number words become digits.
- Smart: converts in context.
- As Transcribed: keeps numbers as spoken.

The English and Devanagari grammars stay out of each other's way, and
romanized Hinglish number words are left alone. Digits paste plain, with no
thousands separators, so code and IDs survive the trip.

### The command matrix

One table feeds every spoken-command surface: command mode, spoken
punctuation, voice deletion. In Settings, under Commands, each of the thirty
commands lists the phrases it listens for and what it inserts. Add a phrase,
remove one, reset a single row, or reset the whole table behind a
confirmation. Duplicate phrases and collisions are flagged on the row before
anything saves.

### Models

The built-in catalog lists 69 models: the Whisper family, Parakeet, Canary,
Moonshine (including per-language builds), Voxtral, Qwen3-ASR, SenseVoice,
GigaAM for Russian, Granite Speech, and more. Each entry lists its languages,
speed and accuracy notes, and whether it can translate. Sizes run from about
35 MB to several GB. On first launch the picker features two recommended
downloads, Parakeet Unified for English and Nemotron Streaming for
multilingual, with three more recommended below them (Canary 180M Flash,
Cohere Transcribe, Whisper Medium).

You are not limited to the catalog. Paste a public Hugging Face repo URL or
owner/repo id, and VoxBar lists the GGUF files in it, checks the
architecture against the supported list, and adds the one you pick. Models
already in your shared Hugging Face cache are picked up without a second
copy. Streaming-capable models are marked Streaming in the picker; the rest
run in batch mode.

Some models (Canary, and others marked Translate) can turn speech in other
languages into English text. The Translate to English toggle is in Settings,
Advanced, and the model picker has a filter for models that support it.

### History

Every dictation is saved with the model that produced it, shown as a compact
badge you can turn off. The audio is kept alongside it as a WAV. Two
retention settings decide what lingers: History Limit (default 5 entries)
can go all the way to 0, which keeps nothing, text or audio; and Auto-Delete
Recordings offers keep-latest-N or time windows of 3 days, 2 weeks, or 3
months, or never. Starred entries are exempt from both. You can
re-transcribe any entry with a different model, copy it, or delete it, and
the recordings folder opens from Settings. A failed transcription keeps its
recording so you can retry it from History.

### Updates

VoxBar updates itself in the app and shows what changed before restarting
into the new version. The policy is yours: ask first (the default), download
in the background and prompt, or install silently. How checks, signing, and
first-download trust work is covered under Updating below.

### Languages

The interface ships in 26 languages, including right-to-left layouts. The
model catalog covers many more. Translation fixes are welcome, and there is
a guide for adding a language.

### Settings

A search box finds any setting by name. Behind it: shortcut behavior (hold
to talk, tap to toggle, or auto, which decides by how long you held the
key), paste behavior (method, delays, auto-submit, trailing space),
recording behavior (VAD, filler-word removal, capture buffer, streaming
tail), hardware acceleration for both engines with Metal picked
automatically on macOS, an option to keep the microphone warm between
back-to-back dictations, overlay style and position, accent color, theme,
sound theme, start hidden, launch at login, and CLI flags like
`--toggle-transcription` for scripting.

## Install

Requirements: an Apple Silicon Mac with macOS 11 (Big Sur) or later. Big
Sur was the first macOS that runs on Apple Silicon, so it is the real floor
for the aarch64 release builds; the bundle metadata still declares 10.15, a
version no Apple Silicon Mac ever ran, and that declaration only matters
for Intel builds you make yourself. There is no fixed RAM requirement: the
memory guard refuses loads that would not fit your free memory, so model
choice is what matches the app to your machine. Intel Macs can build from
source.

1. Download the release zip for macOS from the
   [releases page](https://github.com/nexiouscaliver/voxbar/releases).
2. Unzip it and drag VoxBar.app to Applications.
3. The app is not signed with a developer certificate, so Gatekeeper blocks
   the first launch with an "Apple could not verify" warning. To open it:
   System Settings, Privacy and Security, scroll to the bottom, click
   "Open Anyway" and confirm. Terminal alternative:
   `xattr -dr com.apple.quarantine /Applications/VoxBar.app`
4. Grant Accessibility and Microphone access when prompted. Accessibility is
   what makes the global hotkey and the paste work.
5. On first launch, pick a model and let it download once. The featured
   picks are a good start. Then hold Option+Space and talk.

The Gatekeeper step is one time. From version 1.1.0 on, every further update
installs inside the app (see below) and never repeats it, because the updater
delivers the new version without the browser download that triggers the
check.

## Using it

The default keys on macOS:

| Action                            | Default key                |
| --------------------------------- | -------------------------- |
| Record and transcribe             | Option+Space               |
| Record with polish pass           | Option+Shift+Space         |
| Cancel the current recording      | Esc                        |
| Command mode (hold mid-dictation) | not bound, set in Settings |
| Delete last word                  | not bound, set in Settings |
| Undo (clear the live dictation)   | not bound, set in Settings |

Hold the transcribe key and speak; release and the text is pasted at your
cursor. Tap it instead and it toggles: record until you tap again. Every key
is rebindable in Settings, General, under VoxBar Shortcuts. The delete-word
and undo hotkeys work only while a dictation is live, so they are safe to
bind to keys you use elsewhere.

The tray menu mirrors the state: which model is resident and its RAM, unload
controls, copy the last transcript, and Check for Updates.

## Updating

Updates install in the app: open the tray menu and click "Check for
Updates", or use the link in the settings footer. If a new version is found,
VoxBar shows what changed, downloads the update, and restarts itself into
the new version. The download comes from this project's GitHub releases and
is verified against a signing key embedded in the app before it installs:
each update carries a minisign (ed25519) signature checked against that key.
The signature covers updates only, not the first download, which is a plain
GitHub HTTPS zip (see Install for why Gatekeeper asks once).

Update checks also run quietly at startup when enabled. To turn them off,
disable "Update Check" in Settings, under General. Distributions that repack
VoxBar (the Nix package, for example) can force checks off entirely with the
`HANDY_DISABLE_UPDATER` environment variable, which removes the menu item
and the footer link; the name is upstream Handy branding that still lives in
the code.

For how release artifacts are built and signed, see the "Releasing (macOS)"
section of [BUILD.md](BUILD.md).

## Troubleshooting

**Shortcuts silently stop firing after an update or rebuild.** One macOS
quirk worth knowing: after a rebuild or an app update, macOS may treat the
app as new and dictation shortcuts can silently stop firing even though
Accessibility still shows as enabled. Release builds pin their macOS code
identity to the app's bundle identifier (every update is re-signed against
the same designated requirement), and VoxBar self-checks accessibility at
launch, so this should be rare. If it still happens, quit VoxBar, run
`tccutil reset Accessibility com.voxbar.app` in a terminal, and grant
Accessibility again on the next launch. Microphone and other permissions are
not affected.

**Shortcuts blocked by Secure Input.** If a terminal or security tool has
enabled macOS Secure Keyboard Entry, global shortcuts cannot fire and the
tray menu shows a warning about it. Quit the app that enabled Secure Input
and shortcuts come back.

**Why is the first model download slow?** Models come from Hugging Face and
are a one-time download. They range from about 35 MB (Moonshine Tiny) to
hundreds of MB and up (Whisper Medium is roughly 500 MB, the polish model is
610 MB), and the download speed is whatever the Hugging Face CDN gives your
region. The picker shows live MB/s and a cancel button. Anything already in
your shared Hugging Face cache from another tool is reused, not
re-downloaded.

**The first dictation after boot takes a moment.** That is the model loading
into RAM. After that it stays resident (the tray shows it) until the unload
timer drops it, 2 minutes idle by default.

## Build from source

You need Rust (latest stable, from [rustup](https://rustup.rs/)),
[Bun](https://bun.sh/), and Xcode Command Line Tools
(`xcode-select --install`).

```bash
git clone https://github.com/nexiouscaliver/voxbar.git   # or git@github.com:nexiouscaliver/voxbar.git
cd voxbar
bun install
bun run tauri dev     # development build with hot reload
bun run tauri build   # release build
```

The finished app bundle ends up in `src-tauri/target/release/bundle/`.

On an Intel Mac, prebuilt ONNX Runtime binaries are not available, so build
with `ORT_LIB_LOCATION=$(brew --prefix onnxruntime)/lib
ORT_PREFER_DYNAMIC_LINK=1` in the environment (see
[BUILD.md](BUILD.md) for the full notes, including Windows and Linux
requirements and a Linux install-from-source walkthrough).

## Where your data lives

- App settings, history, and downloaded models:
  `~/Library/Application Support/com.voxbar.app/`
- Models pulled from Hugging Face also use the shared Hugging Face cache at
  `~/.cache/huggingface/hub/`, so a model downloaded by another tool is
  picked up without a second copy.

## Platform status

- macOS: this is what I use every day; it is the tested platform, and
  releases are built for Apple Silicon.
- Windows and Linux: the code carries support for both and
  [BUILD.md](BUILD.md) documents the requirements, but I have not run VoxBar
  on either platform. Treat them as untested.

## Contributing

Bug fixes are the top priority. New features need community support first:
open a discussion before writing code, read the PR template before opening a
pull request, and follow [CONTRIBUTING.md](CONTRIBUTING.md) for the full
workflow. Translation help has its own guide,
[CONTRIBUTING_TRANSLATIONS.md](CONTRIBUTING_TRANSLATIONS.md).

## Credit and license

VoxBar is authored by Shahil Kadia. It was reworked from the
[Handy](https://github.com/cjpais/Handy) project by CJ Pais and its
contributors, and would not exist without their work.

Both projects are MIT licensed. See [LICENSE](LICENSE) for the full text,
including the preserved upstream notice.
