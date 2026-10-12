# VoxBar

VoxBar is fast, private dictation for your Mac. Hold a key, speak, and clean
text lands at your cursor, transcribed and polished entirely on your machine.
Your phone can also be the microphone: pair it once and talk from across the
room, with your Mac as the engine.

By default nothing you say leaves the Mac. There is no cloud service, no
account, and no telemetry, and the transcription model runs locally. The app
talks to the network for three things only: downloading models you ask for,
checking for updates if you leave that on, and the optional cloud polish
providers you would have to configure yourself. The result is pasted straight
into whatever app has focus.

## At a glance

| Capability          | What you get                                                                                                           |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Streaming dictation | Words appear on the overlay as you speak; paste on release                                                             |
| Models              | 69-model catalog plus any public Hugging Face GGUF; streaming marked                                                   |
| Spoken commands     | 30 built-in commands, editable phrase matrix, command mode                                                             |
| Post-processing     | Local LLM, Apple Intelligence, or 8 cloud providers; prompt templates, Prompt Studio, diffs                            |
| Companion phones    | Phones as mic and trigger over Wi-Fi or Tailscale, each in its own session; transcript to the Mac or back to the phone |
| Notifications       | Persistent macOS notifications for errors and outcomes, with an in-app log                                             |
| Tray                | Live status, model RAM, recent dictations, companion state, model and prompt switching                                 |
| Memory guard        | Refuses loads that would not fit; auto-fallback to a smaller model you have                                            |
| Interface           | 26 languages including right-to-left layouts                                                                           |

## Quick start

1. Download the release zip for macOS from the
   [releases page](https://github.com/nexiouscaliver/voxbar/releases), unzip
   it, and drag VoxBar.app to Applications.
2. The app is not signed with a developer certificate, so Gatekeeper blocks
   the first launch. Open it anyway: System Settings, Privacy and Security,
   "Open Anyway". Terminal alternative:
   `xattr -dr com.apple.quarantine /Applications/VoxBar.app`. This is one
   time; in-app updates never repeat it.
3. Grant Accessibility and Microphone access when prompted. Accessibility is
   what makes the global hotkey and the paste work.
4. Pick a model on first launch and let it download once. The featured picks
   are a good start.
5. Hold Option+Space and talk. Release, and the text is pasted at your
   cursor.

The default keys on macOS:

| Action                            | Default key                |
| --------------------------------- | -------------------------- |
| Record and transcribe             | Option+Space               |
| Record with polish pass           | Option+Shift+Space         |
| Cancel the current recording      | Esc                        |
| Command mode (hold mid-dictation) | not bound, set in Settings |
| Cycle post-process template       | not bound, set in Settings |
| Delete last word                  | not bound, set in Settings |
| Undo (clear the live dictation)   | not bound, set in Settings |

Hold the transcribe key and speak; release and the text is pasted. Tap it
instead and it toggles: record until you tap again. Every key is rebindable
in Settings, General, under VoxBar Shortcuts. The delete-word and undo
hotkeys work only while a dictation is live, so they are safe to bind to
keys you use elsewhere.

## Live overlay

While you dictate, a single overlay carries everything: live text, a
waveform, and anything the app needs to tell you. Finished words show in
solid text, the word still forming in lighter text, so you always know what
the model heard. On Parakeet Unified EN 0.6B, the first featured pick, words
show up roughly a third of a second after you say them: VoxBar starts the
stream at a faster setting the model supports instead of the model default.

- A waveform shows the microphone is picking you up, and an elapsed timer
  shows how long you have been going.
- Fix mistakes without the mouse. Delete a word by hotkey or by voice, and
  the overlay shows what it removed on a small struck-through chip.
- A streaming session keeps capturing for 200 ms after you release the key,
  so a quick release does not clip the last word. Adjustable in Output
  settings.
- Batch models like the Whisper family do not stream; they show the
  finished text on the overlay for a beat before the paste.
- The overlay sits at the top or bottom of the screen. Scroll back through
  what you said while still recording; it follows the newest line again
  when you scroll back down.
- A phone badge marks a companion session, and a polish badge names the
  post-process template the session will run.

## Models

The built-in catalog lists 69 models: the Whisper family, Parakeet, Canary,
Moonshine (including per-language builds), Voxtral, Qwen3-ASR, SenseVoice,
GigaAM for Russian, Granite Speech, and more. Each entry lists its
languages, speed and accuracy notes, and whether it can translate. Sizes run
from about 35 MB to several GB. On first launch the picker features two
recommended downloads, Parakeet Unified for English and Nemotron Streaming
for multilingual, with three more recommended below them (Canary 180M Flash,
Cohere Transcribe, Whisper Medium).

You are not limited to the catalog. Paste a public Hugging Face repo URL or
owner/repo id, and VoxBar lists the GGUF files in it, checks the
architecture against the supported list, and adds the one you pick. Models
already in your shared Hugging Face cache are picked up without a second
copy. Streaming-capable models are marked Streaming in the picker; the rest
run in batch mode.

Some models (Canary, and others marked Translate) can turn speech in other
languages into English text. The Translate to English toggle is in Settings,
General, and the model picker has a filter for models that support it.

## Spoken commands

Out of the box, saying "comma" gives you a comma. Thirty commands ship built
in, in four groups:

- Punctuation (9): period, comma, question mark, exclamation, colon,
  semicolon, dash, new line, new paragraph.
- Symbols (16): at sign, hash, dollar sign, percent, star, ampersand,
  caret, open and close parenthesis, bracket, brace, slash, backslash,
  pipe.
- Editing (3): delete word, delete line, clear all.
- Control (2): undo, paste.

Command mode is a modifier key you bind yourself: hold it mid-dictation and
everything you say is treated as commands that edit your text directly. The
overlay shows a badge while it is active. Release to go back to dictating.
A lone modifier binding (left command, for example) engages after a short
150 ms hold so accidental chords never trigger it.

The command engine is careful with plain speech. "A period of time" and
"five star hotel" stay words, while a command word at the end of a sentence
still converts. In command mode, words the engine does not recognize are
discarded rather than pasted. Voice deletion understands counts ("delete
last three words" deletes three), and terminal punctuation can end every
transcript with a period or question mark automatically.

Built-in phrases cover English, Hindi (Devanagari and romanized spellings)
and Chinese. For any other language, add your own phrases in the command
matrix.

### Spoken numbers

Say "one eight zero one" and 1801 lands in your editor. Three modes live in
Settings, Commands, under Spoken Numbers: Digits (the default), Smart
(context-aware), and As Transcribed. The English and Devanagari grammars
stay out of each other's way, romanized Hinglish number words are left
alone, and digits paste plain with no thousands separators, so code and IDs
survive the trip.

### The command matrix

One table feeds every spoken-command surface: command mode, spoken
punctuation, voice deletion. In Settings, under Commands, each of the
thirty commands lists the phrases it listens for and what it inserts. Add a
phrase, remove one, reset a single row, or reset the whole table behind a
confirmation. Duplicate phrases and collisions are flagged on the row
before anything saves.

## Post-processing

Raw transcripts have rough edges. VoxBar can polish them after
transcription using a prompt template you control. Fourteen templates ship
built in, from English Professional and English Minimal to Hindi,
Hinglish, Marathi, Tamil, Spanish, French, German, Japanese, and Chinese,
plus a Prompt Enhancer that turns a dictated instruction into a structured
prompt for an AI agent. Every built-in template opens with a shared
voice-transcription preamble that tells the engine the input is an ASR
transcript: fix homophones, spoken punctuation, and hallucinated fragments
first, never invent content. Write your own templates too, with a
placeholder for the transcript.

Three engine kinds:

| Engine             | Where it runs               | Notes                                                                                                                                                                                                                                                                   |
| ------------------ | --------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Local (on-device)  | Your Mac, through llama.cpp | 6-model catalog: Qwen3 0.6B (the 610 MB default), Qwen3 1.7B, Llama 3.2 1B and 3B, Gemma 3 4B, Phi 3.5 Mini. Add any public language-model GGUF from Hugging Face.                                                                                                      |
| Apple Intelligence | Your Mac, the system model  | Apple Silicon, macOS Tahoe 26.0 or later, enabled in System Settings.                                                                                                                                                                                                   |
| Cloud              | Off device, labeled as such | Presets for OpenAI, Z.AI, OpenRouter, Anthropic, Groq, Cerebras, and AWS Bedrock (Mantle), plus a Custom option for any OpenAI-compatible endpoint such as Ollama. Model lists are fetched from the key, searchable, and each preset carries a Test Connection verdict. |

Turn the polish on and every dictation gets it, or leave it off and use
the dedicated Option+Shift+Space hotkey when a transcript deserves it.
Pressing that hotkey mid-dictation flips the live session's polish on or
off, and the overlay badge names the active template either way. A separate
cycle hotkey (unbound by default) steps through your template library from
the keyboard and confirms the pick on the overlay, showing its position,
for example 2 of 5.

Guardrails keep a dictation from being lost to the polish step:

- The voice model and the local polish model are not in RAM at the same
  time: VoxBar swaps one for the other when the memory gate says the
  footprint fits your free RAM. A keep-warm setting holds the local model
  in memory between polish runs.
- The local pass gives up after 45 seconds; cloud requests default to 60
  seconds (30 for Groq and Cerebras), adjustable 5 to 600 per provider.
- A press of the cancel key aborts either.
- If the engine returns the text unchanged, or the polish cannot run at
  all (low RAM, transcript too long, provider not configured), VoxBar
  pastes the raw transcript and tells you why on the overlay.
- Every run is recorded in a per-run log (Debug tab) with its outcome:
  applied, unchanged, skipped, or failed, plus latency and character
  counts.

### Prompt Studio

Settings, Prompt Studio is an evaluation bench: pick a source transcript
(your last dictation or any history entry), any number of templates, and
any number of engines, then run the whole batch. Each result card shows
the polished text, a diff against the source, the outcome, and the
latency, and can be re-run on its own. Studio runs use the same engine
pipeline as a real dictation, but nothing is typed into other apps and
nothing is saved to history. Each template also has a Test button that
opens the studio preloaded with it.

### History with diffs

Every dictation is saved with the model that produced it, shown as a
compact badge you can turn off. When post-processing changed the text, the
entry shows the final pasted text, copies that final text, and offers a
View changes expander with the raw-to-processed diff. The audio is kept
alongside as a WAV, and each entry records whether it came from the
keyboard or a phone, which is what the phone's own recent list draws
from.

Two retention settings decide what lingers: History Limit (default 5
entries) can go all the way to 0, which keeps nothing; and Auto-Delete
Recordings offers keep-latest-N or time windows of 3 days, 2 weeks, or 3
months, or never. Starred entries are exempt from both. Re-transcribe any
entry with a different model, copy it, or delete it; a failed transcription
keeps its recording so you can retry it from History.

## Companion: your phone as the microphone

Your phone can be the microphone and the trigger, with your Mac as the
engine. Turn on Companion Devices in Settings, Advanced, Experimental, scan
the QR code with the phone camera, and the phone opens a web page with one
big talk button. Pair more than one phone: each runs its own session with
its own recorder and recent list, so two phones can dictate at the same
time and one stopping never cuts the other off. No app to install; the
page is served by your Mac, in all 26 app languages.

The phone page carries per-session controls:

- Send to: To PC pastes on the Mac as before; To Phone delivers the
  transcript back to the phone instead, as a card you can copy (with a
  Polished badge when the polish ran) that also tries the clipboard
  automatically on arrival.
- Polish text: run the post-process leg for this session from the phone.
- The choice persists on the phone and is armed at the moment you press,
  so a session that is already recording keeps the shape it started with.
- Hold to talk, tap to lock, or toggle, matching your keyboard shortcut
  behavior. A recent list on each phone shows that phone's last five
  dictations with copy buttons.

Each phone runs its own recording independently: two phones can dictate at
once, and one phone stopping, dropping off the network, or merely watching
never cuts another session short. The Mac's keyboard still arbitrates with
companion sessions, because the overlay and the paste surface are single
things: a press that arrives while the other side is already recording is
swallowed with a notice on both surfaces. Audio rides the same path the
built-in microphone uses, so the overlay, live text, voice commands,
post-processing, and paste behave the same. When two sessions overlap in
time, both record and transcribe at stop instead of streaming live text.
If the phone leaves the network mid-dictation, the session finishes with
everything captured up to that moment. A session that ends with no audio
at all is called out by name on both surfaces rather than silently pasting
nothing. Sessions cap at 15 minutes with auto-finalize, and a microphone
failure on the phone shows an inline error with a retry button instead of
recording silence.

### Network and listener control

Settings shows every candidate network interface with its OS name and a
classification: LAN, Tailscale, or VPN. Automatic picks the LAN first; a
phone on your tailnet works too, because the Tailscale CGNAT range
(100.64.0.0/10) on a utun interface is a first-class candidate, so pairing
works over Tailscale with no shared Wi-Fi. The range alone is not treated
as proof: a carrier network handing out 100.64.x.x leases on your physical
interface is labeled plain LAN and pairs on its /24 like any LAN. You can
also pin a specific interface, set the port, and override the advertised
host or IP for proxies and odd networks. When the running server no longer
matches the saved configuration, Settings says a restart is pending and
offers the button.

TLS options: the built-in self-signed certificate (fingerprint shown in
Settings so you can check the browser warning the first time), or your own
certificate pair, for example one issued by your tailnet.

Privacy posture: everything runs on your network with no cloud relay. A
pairing token bound to your network gates every phone (a Tailscale bind
issues it for the whole tailnet), failed pairings are rate limited per IP,
input is capped at 2x realtime against ring flooding, and off by default
means no listener and no threads until you switch it on. Reset pairing any
time to revoke phones. Companion devices need macOS this cycle; the phone
page works in mobile Safari and Chrome.

Security note on Tailscale scope: binding the companion server to a
Tailscale interface issues the pairing token for the entire tailnet, not
just your devices. On a shared or organization tailnet, every member of
that tailnet who can reach your Mac and gets the token (or reads it off a
QR you show) can pair as a microphone. Treat the token like a password
into your Mac's dictation, prefer a LAN bind when the phone is on your
Wi-Fi, and reset pairing whenever a device or person leaves the tailnet.

## Notifications

Errors and session outcomes post as real macOS notifications that stay in
Notification Center until you dismiss them, so a failed transcription or a
finished phone session can no longer flash by and be lost. Clicking one
raises the window and navigates: errors land on the Notifications section,
session outcomes on History.

Settings, Notifications carries the controls:

- System notifications: the master toggle, on by default. Off restores the
  previous behavior exactly, including the in-app log.
- Outcome notifications: also notify when a phone session ends (session
  limit, disconnect). Off means errors only.
- Notification log: the last 50 error and outcome notices with a Clear
  all button. New entries are recorded only while system notifications
  are on.

macOS asks for notification permission once, through the app's own
authorization request (System Settings, Notifications, VoxBar). If you
deny it, the app detects that and says so in its log every time a
notification is skipped; re-enable VoxBar there to restore delivery.

## Tray

The tray menu is status-first: what the app is doing right now (Idle,
Recording, Transcribing, with Cancel while busy), then live facts, then
controls.

- A models block lists the resident voice model with its live RAM use, and
  the warm local post-process model when one is held, refreshing every 10
  seconds.
- Recent Dictations lists the last five entries, newest first; clicking
  one copies its final text.
- Copy Last Transcript, and a companion block: On or Off, the paired
  device count, and where the next phone session's transcript will land.
- A Model submenu switches between downloaded models (the label shows the
  resident one and its RAM), Unload Model does it now, and Unload After
  sets the idle timer: presets from 15 seconds to an hour plus immediately
  and never, with 2 minutes the default, or custom.
- Post-process Model and Post-process Prompt submenus switch the local
  polish model and the template without opening the app. The model submenu
  appears while the local engine is the active provider; the prompt
  submenu is disabled while post-processing is off.
- Settings, Check for Updates, and Quit, plus a warning line when macOS
  Secure Input is blocking shortcuts.

## Memory guard and model fallback

VoxBar checks free memory before every model load, and the check
understands macOS memory pressure: reclaimable cache counts when the
system is comfortable and counts less as pressure rises.

If the model you picked will not fit in free RAM, VoxBar does not fail the
dictation. With auto-fallback on (the default), it loads the best smaller
model you already have and names it on the overlay in a neutral chip. A
safety margin setting decides how much spare RAM must remain beyond the
model's size: off by default, with presets from 256 MB to 1536 MB or a
custom number.

## Settings tour

A search box finds any setting by name. Sections, in sidebar order:

| Section       | What lives there                                                                                                                                                                              |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| General       | Shortcuts and shortcut behavior, editing and command-mode keys, model defaults and translate to English, overlay style and position, sounds, microphone choice, start hidden, launch at login |
| Commands      | The command matrix, spoken punctuation, command mode, voice deletion, terminal punctuation, spoken numbers                                                                                    |
| History       | History limit, auto-delete recordings, model badges, open recordings folder                                                                                                                   |
| Notifications | System notifications, outcome notifications, the notification log                                                                                                                             |
| Models        | The catalog, downloads, custom Hugging Face repos, unload controls                                                                                                                            |
| Output        | Paste method and delays, auto-submit, trailing space, capture buffer, streaming tail                                                                                                          |
| Advanced      | Memory guard and headroom, auto-fallback, idle unload, VAD, filler-word removal, hardware acceleration for both engines, experimental features including Companion Devices                    |
| Post Process  | Engine and provider, local model catalog, cloud keys and model lists, timeouts, templates                                                                                                     |
| Prompt Studio | Template and engine evaluation bench                                                                                                                                                          |
| Debug         | Logs, the post-process run table, CLI flags like `--toggle-transcription`                                                                                                                     |
| About         | App language, theme and accent, update checks and update policy                                                                                                                               |

## Install details

Requirements: an Apple Silicon Mac with macOS 11 (Big Sur) or later. Big
Sur was the first macOS that runs on Apple Silicon, so it is the real floor
for the aarch64 release builds; the bundle metadata still declares 10.15,
a version no Apple Silicon Mac ever ran, and that declaration only matters
for Intel builds you make yourself. There is no fixed RAM requirement: the
memory guard refuses loads that would not fit your free memory, so model
choice is what matches the app to your machine. Intel Macs can build from
source.

## Updating

VoxBar updates itself in the app and shows what changed before restarting
into the new version. Open the tray menu and click "Check for Updates", or
use the link in the settings footer. The policy is yours: ask first (the
default), download in the background and prompt, or install silently.
Checks also run quietly at startup when enabled; disable "Update Check" in
Settings, About, to turn them off.

The download comes from this project's GitHub releases and is verified
against a minisign (ed25519) signing key embedded in the app before it
installs. The signature covers updates only, not the first download, which
is a plain GitHub HTTPS zip (see Quick start for why Gatekeeper asks
once). From version 1.1.0 on, every further update installs inside the app
and never repeats the Gatekeeper step, because the updater delivers the
new version without the browser download that triggers the check.

Distributions that repack VoxBar (the Nix package, for example) can force
checks off entirely with the `HANDY_DISABLE_UPDATER` environment variable,
which removes the menu item and the footer link; the name is upstream
Handy branding that still lives in the code.

For how release artifacts are built and signed, see the "Releasing (macOS)"
section of [BUILD.md](BUILD.md).

## Privacy

- Transcription and the local polish pass run on your machine. The models
  are downloaded from Hugging Face once and then work offline.
- No account, no telemetry, no analytics.
- The phone companion link is direct, device to Mac, over your own
  network. No cloud relay sees the audio.
- Cloud polish is opt-in per provider with your own API key, and the
  provider row is labeled off-device.
- Update checks hit GitHub only, and only while enabled.

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
  [BUILD.md](BUILD.md) documents the requirements, but I have not run
  VoxBar on either platform. Treat them as untested.

## Development

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
The interface ships in 26 languages; every user-facing string lands in
English plus all locale files, and a gate enforces parity. Translation
fixes are welcome and have their own guide,
[CONTRIBUTING_TRANSLATIONS.md](CONTRIBUTING_TRANSLATIONS.md).

On an Intel Mac, prebuilt ONNX Runtime binaries are not available, so
build with `ORT_LIB_LOCATION=$(brew --prefix onnxruntime)/lib
ORT_PREFER_DYNAMIC_LINK=1` in the environment (see
[BUILD.md](BUILD.md) for the full notes, including Windows and Linux
requirements and a Linux install-from-source walkthrough).

## Troubleshooting

**Shortcuts silently stop firing after an update or rebuild.** One macOS
quirk worth knowing: after a rebuild or an app update, macOS may treat the
app as new and dictation shortcuts can silently stop firing even though
Accessibility still shows as enabled. Release builds pin their macOS code
identity to the app's bundle identifier (every update is re-signed against
the same designated requirement), and VoxBar self-checks accessibility at
launch, so this should be rare. If it still happens, quit VoxBar, run
`tccutil reset Accessibility com.voxbar.app` in a terminal, and grant
Accessibility again on the next launch. Microphone and other permissions
are not affected.

**Shortcuts blocked by Secure Input.** If a terminal or security tool has
enabled macOS Secure Keyboard Entry, global shortcuts cannot fire and the
tray menu shows a warning about it. Quit the app that enabled Secure Input
and shortcuts come back.

**Why is the first model download slow?** Models come from Hugging Face and
are a one-time download. They range from about 35 MB (Moonshine Tiny) to
hundreds of MB and up (Whisper Medium is roughly 500 MB, the default
polish model is 610 MB), and the download speed is whatever the Hugging
Face CDN gives your region. The picker shows live MB/s and a cancel
button. Anything already in your shared Hugging Face cache from another
tool is reused, not re-downloaded.

**The first dictation after boot takes a moment.** That is the model
loading into RAM. After that it stays resident (the tray shows it) until
the unload timer drops it, 2 minutes idle by default.

**The phone cannot reach the Mac.** The phone and the Mac must share a
network: the same Wi-Fi or LAN, or the same tailnet. Check the Interface
picker in Settings, Advanced, Experimental, Companion Devices; a chosen
interface that is no longer present is reported there instead of silently
rebinding. macOS may also have prompted to allow incoming connections the
first time; allow it, or phones cannot reach this Mac.

**The history database cannot be read.** If the history database is ever
corrupted or unreadable, VoxBar does not fail to launch. It keeps the
broken file beside the new one as `history.db.corrupt` (with a timestamp
suffix when that name is already taken), starts a fresh history at the
same location, and posts one persistent Notification Center notice naming
the quarantined file so you can inspect it or delete it. Your saved audio
WAVs are not touched.

## Contributing

Bug fixes are the top priority. New features need community support first:
open a discussion before writing code, read the PR template before opening
a pull request, and follow [CONTRIBUTING.md](CONTRIBUTING.md) for the full
workflow.

## Credit and license

VoxBar is authored by Shahil Kadia. It was reworked from the
[Handy](https://github.com/cjpais/Handy) project by CJ Pais and its
contributors, and would not exist without their work.

Both projects are MIT licensed. See [LICENSE](LICENSE) for the full text,
including the preserved upstream notice.
