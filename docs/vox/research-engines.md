# Research: the Handy transcription pipeline, engines, and post-processing

Area: `handy-dictation/` (upstream cjpais/Handy, Tauri 2 + Rust + TypeScript offline dictation).
Scope of this note: audio capture → VAD → engine → text → post-process → paste; the engine
abstraction (transcribe-cpp vs transcribe-rs); the post-processing layers; and the seams where a
new engine or an engine-selection policy would plug in. All citations are `path:line` relative to
`handy-dictation/` unless the path is absolute. Everything below was read in this session; nothing
was built or executed beyond the reads/greps named inline.

---

## 1. End-to-end pipeline (one dictation)

Lifecycle owner: `TranscriptionCoordinator` — a dedicated thread running a pure state machine with
stages `Idle → Recording(binding_id) → Processing` (`src-tauri/src/transcription_coordinator.rs:122-128`).
It serializes keyboard/signal/CLI edges, handles push-to-talk vs toggle vs hold-or-toggle
(`classify_busy_input`, `src-tauri/src/transcription_coordinator.rs:97-120`), and executes `Effect::Start`
/ `Effect::Stop` through `ACTION_MAP` (`src-tauri/src/transcription_coordinator.rs:671-710`). The two
transcribe bindings are `"transcribe"` and `"transcribe_with_post_process"`
(`src-tauri/src/transcription_coordinator.rs:537-539`); both map to `TranscribeAction` differing only in
`post_process: bool` (`src-tauri/src/actions.rs:860-880`).

**Start** (`TranscribeAction::start`, `src-tauri/src/actions.rs:388-562`):

1. `tm.initiate_model_load()` (background thread, `src-tauri/src/managers/transcription.rs:777-804`) and
   `rm.preload_vad()` (`src-tauri/src/managers/audio.rs:623-635`) kick off in parallel.
2. Reads `ModelManager::get_model_info(settings.selected_model).supports_streaming` — the _single
   pre-recording source_ for streaming decisions; unknown renders as `false`
   (`src-tauri/src/actions.rs:434-447`).
3. VAD policy: `VadPolicy::Disabled` if `!settings.vad_enabled`, `Streaming` if the model streams,
   else `Offline` (`src-tauri/src/actions.rs:441-447`). Streaming models also get `tm.start_stream()`.
4. `rm.try_start_recording(&binding_id, vad_policy)` opens the mic (on-demand) and returns a
   `RecordingReadiness` one-shot that fires on the first real microphone sample; a spawned thread
   awaits it, emits the ready cue, plays the start chime, and applies mute-while-recording
   (`src-tauri/src/actions.rs:475-529`).

**Capture → VAD** (always running while a stream is open): cpal input stream → wait-free SPSC ring
(`rtrb`) → consumer thread drains ≤50 ms chunks (`src-tauri/src/audio_toolkit/audio/recorder.rs:31-36`,
`914-1043`). `CaptureProcessor` resamples to 16 kHz mono in frames sized for the VAD backend
(`recorder.rs:699-772`), then `handle_frame` routes each frame through the `SmoothedVad` wrapper:
speech frames are appended to the recording buffer _and_ forwarded to the `audio_cb`
(`recorder.rs:631-664`). The `audio_cb` is installed at recorder construction as
`router.feed(frame)` — the `StreamRouter` owned by `TranscriptionManager`
(`src-tauri/src/managers/audio.rs:344-349`, `src-tauri/src/managers/transcription.rs:125-181`), so live
frames reach the streaming worker without touching Tauri state or the manager lock. A closed router
costs one relaxed atomic load per frame (`transcription.rs:168-175`).

**Stop** (`TranscribeAction::stop`, `src-tauri/src/actions.rs:564-820`):

1. `rm.stop_recording()` drains the ring (pause-ack handshake so the boundary block is included),
   flushes the resampler tail through VAD, optionally sleeps `extra_recording_buffer_ms` first, pads
   recordings shorter than 1 s out to 1.25 s of zeros, and returns `Vec<f32>` PCM
   (`src-tauri/src/managers/audio.rs:983-1059`, `recorder.rs:960-1009`).
2. In a tokio task with a `FinishGuard` (unloads model if timeout=Immediately, notifies the
   coordinator, trims freed memory — `src-tauri/src/actions.rs:36-48`): WAV save runs concurrently
   with transcription (`actions.rs:637-660`).
3. Transcription: `tm.finalize_stream()` first — a finalized live stream with non-empty text wins;
   empty/`None` falls back to `tm.transcribe(samples)` batch (`actions.rs:651-660`).
4. Post-processing: `process_transcription_output` → optional LLM polish (`actions.rs:701-718`).
5. History save (`hm.save_entry`) if the WAV verified, then **paste on the main thread** via
   `utils::paste(final_text, …)` inside `run_on_main_thread` (`actions.rs:744-774`).
6. `paste()` dispatches on `settings.paste_method`: `None | Direct | CtrlV | CtrlShiftV |
ShiftInsert`; on macOS/Windows with `reliable_paste` it first tries the receipt-sequenced
   clipboard paste (lazy NSPasteboard promises, restore only after the target actually read the
   clipboard) and falls back to the legacy timed restore (`src-tauri/src/clipboard.rs:774-830`;
   `src-tauri/src/paste_tx/mod.rs:1-52`).

Cancellation flows through a `cancel_generation` atomic checked at every stage
(`src-tauri/src/managers/audio.rs:975-981`, `src-tauri/src/actions.rs:621-725`).

---

## 2. VAD layer

- Trait: `VoiceActivityDetector` with `push_frame`/`frame_samples`/`set_hangover_frames`/
  `tail_report`/`reset` (`src-tauri/src/audio_toolkit/vad/mod.rs:36-66`).
- Two backends, selected by `settings.vad_backend` (`VadBackend::Silero | Earshot`):
  - **Silero** — ONNX model loaded from bundled `resources/models/silero_vad_v4.onnx` via the
    `vad-rs` crate (cjpais fork, default-features off — `Cargo.toml:59`); 30 ms / 480-sample frames;
    threshold 0.3 (`src-tauri/src/audio_toolkit/vad/silero.rs:9-56`;
    `src-tauri/src/managers/audio.rs:20, 286-299`).
  - **Earshot** — pure-Rust `earshot = "1.2.2"` crate (`Cargo.toml:60`); 16 ms / 256-sample frames;
    threshold 0.5; clamps resampler overshoot before prediction (`src-tauri/src/audio_toolkit/vad/earshot.rs:5-72`;
    `src-tauri/src/managers/audio.rs:21, 300-303`).
- Both are wrapped in **`SmoothedVad`** (onset confirmation, pre-roll buffer, post-speech hangover)
  (`src-tauri/src/audio_toolkit/vad/smoothed.rs:13-164`). Timing profile is defined in _milliseconds_
  and converted per-backend by rounding up so switching backends never shortens audio:
  `VAD_PREFILL_MS = 450`, `VAD_OFFLINE_HANGOVER_MS = 450`, `VAD_STREAMING_HANGOVER_MS = 1650`,
  `VAD_ONSET_MS = 60` (`src-tauri/src/audio_toolkit/vad/mod.rs:4-8`). One detector instance is
  reconfigured per session for offline vs streaming hangover (`recorder.rs:60-81`, `774-791`).
- End-of-recording `tail_report()` is a diagnostic (withheld frames / voiced frames) logged at stop
  (`recorder.rs:884-901`).
- VAD backend can be swapped at runtime while idle (`AudioRecordingManager::update_vad_backend`,
  `src-tauri/src/managers/audio.rs:868-916`).

---

## 3. Engine abstraction

`TranscriptionManager` holds **two engine homes** (`src-tauri/src/managers/transcription.rs:243-276`):

1. `engine: EngineSupervisor` — transcribe-cpp, **out-of-process** (own worker binary invocation).
2. `onnx: Arc<Mutex<Option<OnnxEngine>>>` — transcribe-rs ONNX engines, **in-process**
   (`OnnxEngine` enum: Parakeet, Moonshine, MoonshineStreaming, SenseVoice, GigaAM, Canary, Cohere —
   `transcription.rs:186-194`).

The routing discriminant is `ModelInfo.engine_type` (`EngineType` enum:
`TranscribeCpp, Parakeet, Moonshine, MoonshineStreaming, SenseVoice, GigaAM, Canary, Cohere` —
`src-tauri/src/managers/model.rs:26-39`). At load time (`load_model_with_device`,
`transcription.rs:518-774`) the match at `transcription.rs:606-744` dispatches:
`TranscribeCpp` → `EngineSupervisor.load(LoadSpec{path, backend, device})`; every other variant →
the corresponding `transcribe_rs::onnx::*::load(&model_path, &Quantization::Int8)` (Moonshine uses
`Quantization::default()`), stored in the `onnx` mutex.

At run time (`transcribe`, `transcription.rs:1128-1251`): `self.engine.loaded()` is `Some` →
`transcribe_cpp(...)`; `None` → `transcribe_onnx(...)` (`transcription.rs:1189-1194`). The ONNX path
takes the engine _out_ of the mutex during the call, wraps it in `catch_unwind` (a panicking engine
is dropped, effectively unloaded, never poisons the mutex — `transcription.rs:1361-1499`), then
returns it via `return_engine` unless the model was switched mid-run (`transcription.rs:1053-1065`).

### 3.1 transcribe-cpp side (whisper family & friends, GGUF/GGML)

- **Process isolation**: all transcribe.cpp native code (backend init, device enumeration, model
  load, inference, streaming) runs in a child process — the Handy executable relaunched with
  `--transcribe-worker`; `--cpu-only` restricts to CPU backends
  (`src-tauri/src/engine_supervisor/mod.rs:1-40`). `EngineSupervisor` owns the single worker slot
  through a command queue on an owner thread (`supervisor.rs:1-15, 324-354`); at most one
  model-holding worker lives at a time, and listing devices may briefly run a second model-less
  worker (`EngineSupervisor::devices`, `supervisor.rs:471-477`).
- **Wire protocol**: strict request/response frames `[u32 json_len][json][u32 pcm_len][pcm f32 LE]`
  (`protocol.rs:1-7`). Requests: `Hello{list_devices}`, `Load{path,backend,device}`, `Run{options}`
  (+PCM), `StreamBegin`, `Feed` (+PCM), `Finalize{want_language}`, `StreamReset`
  (`protocol.rs:22-53`). `LoadedInfo` returns arch/variant/backend/device/on_gpu/capabilities/
  supports_initial_prompt read from the loaded model (`protocol.rs:145-156`; populated in
  `worker.rs:255-316`).
- **Crash/hang recovery**: a worker crash or deadline miss is retried once in a fresh worker — on
  CPU if it was on GPU, and a GPU fault marks the GPU unavailable until an explicit accelerator
  change calls `retry_gpu` (`supervisor.rs:724-756, 786-803`; `EngineSupervisor::retry_gpu`,
  `supervisor.rs:390-394`). Deadlines scale with audio length and are much looser on CPU
  (`GPU_DEADLINES` 30 s floor / 10× audio; `CPU_DEADLINES` 120 s floor / 20× audio —
  `supervisor.rs:68-80`). Cancel kills the in-flight worker outright (`supervisor.rs:454-464`).
- **Device selection**: `Backend::{Auto, Cpu}` + `DeviceSelector::{Auto, Key, Index}`
  (`protocol.rs:58-66`). Settings carry a stable device _key_ (device_id or name for Metal);
  `resolve_gpu_device` maps the persisted GPU choice to a key, `select_transcribe_backend` maps
  Auto/Cpu/Gpu → Backend (`transcription.rs:1996-2030`). `load_model_with_device` additionally
  accepts a hard `device_index` (the `--device-index` CLI flag) that never falls back to CPU
  (`transcription.rs:522-632`; `LoadSpec::pinned`, `supervisor.rs:117-122`).
- **Streaming**: `EngineSupervisor::start_stream(run, stream, on_progress)` returns a `StreamHandle`
  (`supervisor.rs:417-448`); feeds are queued in order with finalize (`StreamHandle::feed/finalize`,
  `supervisor.rs:504-526`). In `TranscriptionManager::run_stream_worker`
  (`transcription.rs:865-1049`) the worker waits out any model load, requires a loaded transcribe-cpp
  model with `capabilities.supports_streaming` (otherwise drains until finalize and the caller falls
  back to batch — `transcription.rs:886-911`), then converts progress to `StreamTextEvent`
  (committed/tentative) for the live overlay, running `PreviewScript` Chinese-script conversion on
  the fly (`transcription.rs:1832-1880`).
- **Run options**: task/language/target_language built by `transcribe_cpp_run_plan` — a language
  hint is only passed when the loaded model advertises it; translate-to-English only when the model
  advertises translation and the source isn't English (`transcription.rs:1723-1757, 1902-1925`).
  Custom words become the whisper `initial_prompt` **only** for `arch == "whisper"` (non-whisper
  archs reject the whisper-kind extension with INVALID_ARG — `transcription.rs:1289-1296`,
  comment at `1513-1527`).
- **Capability truth**: the loaded model's GGUF-derived capabilities overwrite the registry view
  after load via `ModelManager::set_runtime_capabilities` (`transcription.rs:646-657`;
  `model.rs:1293-1328`) — this is what gates streaming attempts and language coercion.

### 3.2 transcribe-rs side (ONNX)

- In-process; per-variant parameter mapping in `transcribe_onnx` (`transcription.rs:1346-1508`):
  Parakeet uses `ParakeetParams{timestamp_granularity: Segment}`; SenseVoice maps the validated
  language to `{zh,en,ja,ko,yue}` with `use_itn: true`; Canary takes
  `TranscribeOptions{language, translate}`; Cohere/GigaAM/Moonshine take language/default options.
- ONNX accelerator preference applied globally via `transcribe_rs::accel::set_ort_accelerator`
  on startup and before each load (`apply_accelerator_settings`, `transcription.rs:2038-2057`).
- ONNX engines never stream: streaming is a transcribe-cpp-only capability
  (`transcription.rs:883-895`); a streaming-configured session with an ONNX model silently records
  batch-style and finalizes through the drain path.
- A headless `--transcribe-file` CLI path drives the same `TranscriptionManager::transcribe`
  (`src-tauri/src/lib.rs:427-436`).

### 3.3 What is compiled in on macOS

From `src-tauri/Cargo.toml` (base deps at `Cargo.toml:79-86`, lockfile pins at
`src-tauri/Cargo.lock:7287-7311` = transcribe-cpp 0.3.1 + transcribe-cpp-sys 0.3.1, transcribe-rs
0.3.8):

- **transcribe-rs 0.3.8, `features = ["onnx"]`** on every platform (`Cargo.toml:83`) — ONNX is
  CPU-only on macOS/Linux/Windows (the comment at `Cargo.toml:112-119` documents dropping
  DirectML on Windows; there is no CoreML EP feature anywhere in the manifest).
- **transcribe-cpp 0.3.1, `default-features = false, features = ["serde"]`** base (`Cargo.toml:84-86`),
  and for `[target.'cfg(target_os = "macos")']`: `features = ["metal"]` (`Cargo.toml:157-159`) —
  i.e. on macOS the ggml **Metal backend is compiled in statically** (no `dynamic-backends`), so
  whisper-family + all GGUF archs run on Metal (or CPU) inside the worker process.
  Compare: Windows x86_64 adds `dynamic-backends` + `vulkan` (`Cargo.toml:148-152`), Windows
  aarch64 is static CPU-only (`Cargo.toml:154-155`), Linux `dynamic-backends` + `vulkan`
  (`Cargo.toml:167-173`).
- **Apple Intelligence (post-processing only, not ASR)**: compiled only for
  `all(target_os = "macos", target_arch = "aarch64")` — `build.rs` compiles
  `swift/apple_intelligence.swift` (FoundationModels `SystemLanguageModel`, `@available(macOS 26.0, *)`)
  into `libapple_intelligence.a` and links it, with automatic fallback to
  `swift/apple_intelligence_stub.swift` when the SDK/CLT lacks FoundationModels or
  `HANDY_FORCE_AI_STUB=1` (`src-tauri/build.rs:385-530`; `src-tauri/swift/apple_intelligence.swift:1-77`).
  The Rust FFI wrapper is `src-tauri/src/apple_intelligence.rs:19-73`.

### 3.4 Model inventory: catalog (GGUF/transcribe-cpp) vs legacy table (mixed)

Two producers feed one registry (`HashMap<String, ModelInfo>` in `ModelManager`,
`model.rs:542-551`):

1. **Bundled catalog** — `src-tauri/src/catalog/catalog.json` (69 models, generated at build time by
   `scripts/gen_catalog.py` from the `handy-computer` HF org; compiled into the binary via
   `include_str!` — `catalog/mod.rs:1-14, 117-124`). Every catalog entry is
   `EngineType::TranscribeCpp` with `ModelSource::HuggingFace{repo_id, revision}` pinned to a commit
   sha (`catalog/mod.rs:82-93`), carrying per-quant `files[]` (Q4_K_M…F32 with sha256), speed/accuracy
   scores 0-100 normalized to 0.0-1.0, `recommended_rank` and `recommended` flags. `ModelDescriptor`
   (`model.rs:167-259`) is the normalised shape; the surfaced id is `"{repo_id}/{filename}"` of the
   default quant. Seeded additively before the disk scans (`seed_catalog_models`, `model.rs:1212-1222`,
   called at `model.rs:1149`). Download goes through hf-hub with a mirror fallback
   (`blob.handy.computer`, `catalog/mod.rs:126-174`) verified against the catalog sha256.
2. **Legacy hardcoded table** in `ModelManager::new` (`model.rs:564-1143`): whisper small/medium/
   turbo/large + breeze-asr as `EngineType::TranscribeCpp` `ModelSource::Url` .bin files, plus the
   **ONNX directory models**: `parakeet-tdt-0.6b-v2` (451 MB, `EngineType::Parakeet`),
   `parakeet-tdt-0.6b-v3` (456 MB, `EngineType::Parakeet`, 25 EU languages, the legacy table's only
   `is_recommended: true` — `model.rs:786-816`), moonshine base + tiny/small/medium streaming,
   sense-voice-int8, gigaam-v3, canary-180m-flash, canary-1b-v2, cohere-int8. Catalog and legacy stay
   **separate** (different files, ids, runtimes); the UI hides not-downloaded legacy `Url` entries
   (`model.rs:1206-1211`).
3. **Disk discovery** (additive): custom `.bin`/`.gguf` files dropped in the models dir
   (`discover_custom_transcribe_models`, `model.rs:1602-1757`) and GGUFs in the shared HF cache
   (`discover_hf_cache_models`, `model.rs:1764-1902`), both probed via the pure-Rust GGUF header
   reader (`GgufHeaderProber`, `src-tauri/src/managers/model_capabilities.rs:148-216`) which reads
   `general.architecture`, `stt.variant`, `general.languages`, `stt.capability.{streaming,translate,
lang_detect}` from the first ≤64 KiB (grown geometrically to 16 MiB). Files matching a catalog
   quant surface with full catalog metadata (`file_in_catalog`, `catalog/mod.rs:180-197`).

### 3.5 Parakeet variants available

**GGUF (transcribe-cpp, catalog.json)** — arch `parakeet` unless noted; accuracy/speed are the
catalog's 0-100 scores; size is the default Q4_K_M quant:

| Model (repo id)                        | langs | acc/speed | streaming | size (Q4_K_M) | catalog line        |
| -------------------------------------- | ----- | --------- | --------- | ------------- | ------------------- |
| parakeet-unified-en-0.6b               | en    | 90/79     | **yes**   | 455 MB        | `catalog.json:9`    |
| nemotron-3.5-asr-streaming-0.6b        | 28    | 82/84     | **yes**   | 472 MB        | `catalog.json:42`   |
| parakeet-tdt-0.6b-v3                   | 25 EU | 88/79     | no        | 462 MB        | `catalog.json:207`  |
| parakeet-tdt-0.6b-v2                   | en    | 89/85     | no        | 453 MB        | `catalog.json:240`  |
| nemotron-speech-streaming-en-0.6b      | en    | 86/80     | **yes**   | 453 MB        | `catalog.json:1377` |
| parakeet-tdt_ctc-110m                  | en    | 85/98     | no        | 85 MB         | `catalog.json:1410` |
| multitalker-parakeet-streaming-0.6b-v1 | en    | 86/96     | **yes**   | 455 MB        | `catalog.json:1443` |
| parakeet-ctc-0.6b                      | en    | 88/94     | no        | 447 MB        | `catalog.json:1482` |
| parakeet-rnnt-0.6b                     | en    | 90/84     | no        | 454 MB        | `catalog.json:1515` |
| parakeet-ctc-1.1b                      | en    | 88/83     | no        | 780 MB        | `catalog.json:1548` |
| parakeet-primeline                     | 25 EU | 67/79     | no        | 462 MB        | `catalog.json:1581` |
| parakeet-tdt-1.1b                      | en    | 91/76     | no        | 787 MB        | `catalog.json:1614` |
| parakeet-rnnt-1.1b                     | en    | 91/75     | no        | 787 MB        | `catalog.json:1647` |
| parakeet-tdt_ctc-1.1b                  | en    | 88/75     | no        | 787 MB        | `catalog.json:1680` |

The two recommended-ranked streaming Parakeets are the catalog's #1 and #2 entries overall
(`recommended_rank` 1 and 2, `recommended: true`; values read from catalog.json this session).
Each ships 6 quant files (Q4_K_M/Q5_K_M/Q6_K/Q8_0/F16/F32; e.g. parakeet-unified-en-0.6b F32 = 2358 MB).

**ONNX (transcribe-rs, legacy table)**: `parakeet-tdt-0.6b-v2` (451 MB int8, acc 0.85/speed 0.85,
en — `model.rs:744-774`) and `parakeet-tdt-0.6b-v3` (456 MB int8, acc 0.80/speed 0.85, 25 EU
languages, `is_recommended` — `model.rs:786-816`). Both load via
`ParakeetModel::load(&path, &Quantization::Int8)` (`transcription.rs:675-684`).

Quirks that matter for Parakeet specifically:

- Parakeet V3 (ONNX) always auto-detects and ignores Handy's language selection — the
  output-language evidence code explicitly refuses to treat an unapplied hint as evidence
  (`transcription.rs:1679-1695`; test `ignored_user_language_is_not_output_evidence`,
  `transcription.rs:2412-2432`).
- Parakeet-family streaming is _inferred_ by transcribe-cpp's loader from encoder hparams, not a
  flat GGUF bool, so pre-download header probes leave it `None` (unknown) and post-load
  reconciliation settles it (`model_capabilities.rs:121-125`; `model.rs:1296-1305`).

### 3.6 Engine/model selection policy (today)

- Single persisted knob: `settings.selected_model` (`src-tauri/src/settings.rs:415`). The frontend
  sets it via the `set_active_model` command → `switch_active_model`, which persists first, then
  eagerly loads unless unload-timeout is "Immediately", reverting the persisted selection on load
  failure (`src-tauri/src/commands/models.rs:99-163`).
- **Auto-selection** (`auto_select_model_if_needed`, `model.rs:1544-1597`): (a) clears the selection
  if the model's files vanished (must be `is_downloaded`, `model.rs:1514-1518`); (b) skipped until
  onboarding is complete; (c) when empty, picks **the first `is_downloaded` model in
  `get_available_models` order** — i.e. catalog `recommended_rank` ascending, then `is_recommended`,
  then accuracy desc, speed desc, name (`model.rs:1184-1202` + `crate::catalog::rank_of`,
  `catalog/mod.rs:199-212`). There is no language-, RAM-, or audio-length-aware policy anywhere;
  "policy" today = this sort + the catalog's editorial rank.
- Recording start refuses to open the mic if no model can transcribe (model path lookup fails)
  (`actions.rs:407-418`).
- Language _intent_ coercion (`effective_language`, `model.rs:272-312`): base-language matching with
  nb→no / fil→tl aliases, falling back to auto (if the model detects language) or English/first
  listed; never written back to settings.

---

## 4. Post-processing layers

Order of operations on the final text (both streaming-finalize and batch) is
`post_process_transcription_text` (`transcription.rs:1759-1821`), itself wrapped in
`fail_open_text_transform` so a panic in cleanup returns the raw text (`transcription.rs:1885-1900`):

1. **Output-language evidence** — `OutputLanguageEvidence` ladder: TranslatedToEnglish >
   UserSelected > ModelConstrained > ModelDetected (from the run) > TextDetected (whatlang on the
   text, confidence-gated and constrained to the model's languages —
   `src-tauri/src/audio_toolkit/lang_id.rs:57-92`) > Unknown (`transcription.rs:1669-1721`).
2. **Chinese script conversion** (ferrous-opencc, `Cargo.toml:88`) when the output is known Chinese
   and `settings.chinese_script != AsTranscribed` (`transcription.rs:1792-1800`;
   `src-tauri/src/chinese_script.rs`).
3. **Custom dictionary** — `apply_custom_words` (`src-tauri/src/audio_toolkit/text.rs:151-220`):
   fuzzy correction of 1–3-word n-grams against `settings.custom_words` using normalized
   Levenshtein + Soundex phonetic boost (`text.rs:79-134`), ASCII-only keys (CJK terms skipped,
   `text.rs:30-60`), threshold `settings.word_correction_threshold`, case/punctuation preserved.
   Skipped when the words were already given to a whisper model as `initial_prompt`
   (`transcription.rs:1802-1810`, gating flag `model_is_whisper`).
4. **Filler-word removal** — two-tier built-ins: universal tokens (`uh`, `uhm`, …) always;
   language-gated tokens (`um/ah/eh` for en, `äh/ähm` de, `euh` fr) only with language evidence;
   `custom_filler_words: Some(vec)` replaces both tiers, `Some(empty)` disables
   (`text.rs:300-321, 426-464`). Sentence-opening capitals are handed to the next word
   (`text.rs:391-405`).
5. **Normalization** — collapse 3+ repeated words (stutters), collapse multi-space, trim
   (`text.rs:327-362, 467-478`).

Then, only for the `transcribe_with_post_process` binding, the **LLM polish**
(`post_process_transcription`, `src-tauri/src/actions.rs:121-345`):

- Requires an active provider (`settings.active_post_process_provider()`), a configured model, and a
  selected non-empty prompt; blank transcriptions skip the call (`actions.rs:121-175`).
- Providers (default list at `settings.rs:672-760`): OpenAI, Z.AI, OpenRouter, Anthropic, Groq,
  Cerebras, **Apple Intelligence** (macOS arm64 only, `base_url: "apple-intelligence://local"`),
  AWS Bedrock (Mantle), Custom (`http://localhost:11434/v1` default, e.g. Ollama).
- Structured-output providers get a JSON schema `{transcription: string}` and the JSON is parsed
  back out, with think-block stripping and invisible-char stripping on every path
  (`actions.rs:191-307`); failures fall back to the legacy `${output}` prompt-substitution mode
  (`actions.rs:310-345`).
- **`llm_client.rs`** is a hand-rolled OpenAI-compatible chat-completions client (reqwest):
  provider-specific auth headers (Anthropic `x-api-key` vs Bearer — `llm_client.rs:139-173`),
  reasoning-disable fields per endpoint flavor (DeepSeek `thinking:{type:disabled}`, OpenRouter
  nested `reasoning:{effort:none,exclude:true}`, default `reasoning_effort:"none"`) with one
  400/422-triggered retry without them and a per-(base_url,model) rejection memo
  (`llm_client.rs:60-110, 404-440`), sanitized URL/error reporting that never echoes response
  payloads (`llm_client.rs:244-295`), and a `/models` list fetch (`llm_client.rs:466-528`).
- **`apple_intelligence.rs`** is the FFI shim over the Swift bridge
  (`process_text_with_system_prompt`, `apple_intelligence.rs:33-73`); availability is checked lazily
  at use time, never at startup (SIGABRT risk on macOS 26 betas — comment at `settings.rs:724-728`;
  check at `actions.rs:199-241`). The "model" field is repurposed as a word-count token limit
  (`actions.rs:210-215`).

---

## 5. Seams: where a new engine or selection policy plugs in

1. **New GGUF-family engine (whisper/parakeet/voxtral-style)**: zero Rust code if transcribe-cpp
   already knows the arch — add a `catalog.json` entry (regenerated by `scripts/gen_catalog.py`,
   `catalog/mod.rs:3-7`) and it flows through `ModelDescriptor` → registry → `EngineType::TranscribeCpp`
   load path. If the arch string is new to Handy's prober, add it to `KNOWN_ARCHES`
   (`src-tauri/src/managers/model_capabilities.rs:29-51`) so header-probed discovery doesn't mark it
   `MaybeIncompatible` (which hides HF-cache discoveries — `model.rs:1857-1861`); transcribe-cpp
   itself must ship the arch, else loads fail in the worker.
2. **New ONNX engine**: (a) extend `EngineType` (`model.rs:26-39`), (b) add a variant to `OnnxEngine`
   (`transcription.rs:186-194`), (c) a load arm in `load_model_with_device`
   (`transcription.rs:606-744`), (d) a run arm in `transcribe_onnx` (`transcription.rs:1377-1457`),
   (e) a listing entry (catalog can't express it — catalog is hardcoded TranscribeCpp at
   `catalog/mod.rs:93` — so legacy-table style in `ModelManager::new` or a new producer).
3. **A whole new runtime (not transcribe-cpp/ONNX)**: mirror the `EngineSupervisor` pattern (own
   worker process + framed protocol) or the in-process `OnnxEngine` slot; `TranscriptionManager`
   (`transcription.rs:243-276`) is the aggregation point and `EngineType` remains the single
   discriminant consumed by both load and run paths. The `CapabilityProber` trait
   (`model_capabilities.rs:148-151`) is the documented substitution seam for pre-download capability
   reads.
4. **Engine-selection policy**: the decision points are exactly
   - `ModelManager::auto_select_model_if_needed` (`model.rs:1544-1597`) — swap the
     "first downloaded in ranked order" `find` for a policy fn;
   - the sort in `get_available_models` (`model.rs:1184-1202`) + `catalog::rank_of`
     (`catalog/mod.rs:199-212`) which define what "ranked order" means;
   - `switch_active_model` (`src-tauri/src/commands/models.rs:99-163`) for explicit selection
     (frontend/tray entry point);
   - `actions.rs:407-418` for the "nothing can transcribe" gate.
     Everything downstream keys off `settings.selected_model` + `ModelInfo` fields
     (`accuracy_score`, `speed_score`, `supported_languages`, `supports_streaming`,
     `supports_language_detection` — `model.rs:60-82`), so a policy has rich inputs without touching
     the engine layer.
5. **Per-session engine choice** (e.g. pick model by detected language or audio length): the natural
   injection point is `TranscribeAction::start` before `initiate_model_load` (`actions.rs:392-399`)
   and `TranscriptionManager::transcribe`'s "load if not loaded" wait (`transcription.rs:1150-1161`);
   note `load_model_with_device` already supports caller-driven one-shot loads without persisting
   selection (`transcription.rs:522-531`), and the headless `--transcribe-file --model` path is a
   working example of loading a non-selected model (`lib.rs:427-436`, `460-470`).

---

## 6. Risks / gotchas

- **In-process ONNX engines have no crash isolation** — the whole mitigation is `catch_unwind` +
  drop-the-engine (`transcription.rs:1374-1499`); a native abort inside ort would take the app down,
  unlike transcribe-cpp's worker process (`engine_supervisor/mod.rs:1-8`). A new engine should
  strongly prefer the worker-process pattern.
- **Streaming is transcribe-cpp-only** and gated on the _registry's_ `supports_streaming` at
  recording start (`actions.rs:434-450`) — which is `false` until either the catalog says so or
  `set_runtime_capabilities` reconciles after a load. A fresh GGUF without the flat metadata key
  will not stream on its first session even if the arch supports it (parakeet streaming is inferred
  at load — `model_capabilities.rs:121-125`).
- **Two model producers coexist**: legacy hardcoded table (mixed engines, `Url` sources, ONNX
  directories) vs catalog (GGUF-only, HF-pinned). They dedupe by id but never merge
  (`model.rs:1206-1222`); any new listing/policy code must handle both `ModelSource` variants
  (`get_model_path` branches per source, `model.rs:2586-2661`).
- **Parakeet V3/ONNX ignores language hints**; evidence-aware post-processing depends on the hint
  having been applied (`transcription.rs:1679-1695`). Mislabeling a model's
  `supports_language_detection` would coerce "auto" to a forced language semi-permanently (the
  warning at `model.rs:1296-1305`).
- **Apple Intelligence build matrix**: real Swift path needs full Xcode (FoundationModelsMacros
  plugin) on arm64 macOS; CLT-only toolchains silently build stubs (`build.rs:416-461`), and
  availability is checked lazily at use (`actions.rs:199-241`). Don't assume the provider works just
  because it compiled.
- **macOS Metal is statically compiled** (no dynamic backends): a GPU driver fault kills the worker;
  recovery retries CPU and marks the GPU unavailable until an explicit accelerator change
  (`supervisor.rs:724-756`). First-listing may pay ~15 s Metal shader compilation on a cold cache
  (`supervisor.rs:36-38`), which is why device listing happens off the startup path
  (`report_compute_devices` from a background thread, `transcription.rs:1944-1971`).
- **Catalog scores are editorial** (0-100 from the generator, normalized to 0-1 in
  `catalog/mod.rs:106-108`); the legacy table's scores are hand-assigned and not comparable in
  provenance. An accuracy-driven selection policy should treat them as ranking hints, not
  measurements.
- **One model at a time, one stream at a time**: `EngineSupervisor` enforces a single worker slot
  (`supervisor.rs:4-8`) and `TranscriptionManager` prevents overlapping stream workers via the
  `active_stream_worker` id (`transcription.rs:844-863`); a policy that wants parallel
  warm engines (e.g. fast + accurate) has no support today — loads replace each other and drop the
  old engine first to bound peak memory (`transcription.rs:590-598`).
- **`auto_select_model_if_needed` is onboarding-gated** (`model.rs:1566-1572`): a fresh install with
  a model already in the shared HF cache will not select it until onboarding completes.

## 7. Not verified / out of scope

- I did not compile anything, run tests, or run the app; all findings are from reading source this
  session. The catalog table in §3.5 was produced with a read-only `python3 -c` JSON parse of
  `catalog.json` (command in session log).
- transcribe-cpp / transcribe-rs internals (which archs each version supports, ONNX EP matrix) are
  only known from Handy's usage sites and Cargo comments, not from reading those crates' sources —
  they are registry dependencies, not in this repo.
- Frontend model-picker UI (`src/`) was only spot-checked for engine_type usage; no line-level
  frontend analysis was done.
