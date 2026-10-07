# Research: Model state in handy-dictation (upstream cjpais/Handy)

Area: where models live on disk, the load/stay-resident/unload lifecycle
(`ModelUnloadTimeout`), how to query the loaded model from Rust and the
frontend, RAM of a loaded model, and where a memory-pressure check could hook
in before a load.

Scope: the `handy-dictation/` checkout inside mission-control (upstream
`cjpais/Handy`, Tauri 2 + Rust + TypeScript, offline dictation app). All path
references below are relative to `handy-dictation/`. Companion note:
`docs/vox/research-build.md` (toolchain/CI).

Everything below was read from source during this research session unless
explicitly marked otherwise. No builds or tests were run (research-only task).

---

## 1. Architecture: three owners of model state

| Owner | File | Owns |
|---|---|---|
| `ModelManager` | `src-tauri/src/managers/model.rs:542-551` | The **registry**: the list of known models (`HashMap<String, ModelInfo>`), download/delete, on-disk discovery, disk-status flags |
| `TranscriptionManager` | `src-tauri/src/managers/transcription.rs:243-276` | The **loaded model**: `current_model_id`, the engine (in two possible homes, below), the idle watcher thread, the `is_loading` gate |
| `EngineSupervisor` | `src-tauri/src/engine_supervisor/supervisor.rs:325` | The **transcribe-cpp worker process**: spawns/kills the child, owns `LoadSpec`/`Unloading`, snapshot of `LoadedInfo` |

Two fundamentally different engine homes, decided by `EngineType`
(`src-tauri/src/managers/model.rs:26-39`):

- `EngineType::TranscribeCpp` (any GGUF/GGML model: whisper family, parakeet
  GGUF, voxtral, qwen3-asr, nemotron, custom `.bin`/`.gguf`): the model runs in
  a **child process** — the Handy executable relaunched with
  `--transcribe-worker` (`src-tauri/src/engine_supervisor/mod.rs:36`). Its RAM
  lives in that worker process.
- All other `EngineType`s (Parakeet/Moonshine/MoonshineStreaming/SenseVoice/
  GigaAM/Canary/Cohere — the legacy ONNX `.tar.gz` directory models): the model
  loads **in the app process** behind `Arc<Mutex<Option<OnnxEngine>>>`
  (`src-tauri/src/managers/transcription.rs:186-194, 249`). Its RAM is in the
  main process.

Module doc, key sentence: *"At most one worker holds a model at a time, and it
lives exactly as long as that model, so unloading returns all of its CPU and
GPU memory to the OS"* (`src-tauri/src/engine_supervisor/mod.rs:8-15`).

At most **one** model is resident at a time, enforced in two places:
`EngineSupervisor::load` — *"The old worker has fully exited before the new one
starts, so two models are never held at once"*
(`src-tauri/src/engine_supervisor/supervisor.rs:362-368`) — and the load path
drops the previous ONNX engine before building the new one, with an explicit
peak-memory comment: *"Drop the current engine BEFORE building the new one …
avoids holding two models at once (peak memory on large GGUFs)"*
(`src-tauri/src/managers/transcription.rs:590-598`).

---

## 2. Where models are stored and downloaded from

### 2.1 On-disk locations

- **App models dir**: `portable::app_data_dir(app_handle)/models`, created at
  `ModelManager::new` (`src-tauri/src/managers/model.rs:554-562`). In normal
  installs this is the OS app-data dir (macOS `~/Library/Application
  Support/<bundle>/models`); in portable mode it is `<exe-dir>/Data/models`
  (`src-tauri/src/portable.rs:83-89`).
- **Shared Hugging Face cache** (the primary path for catalog models):
  hf-hub's `Cache::from_env()`, i.e. `$HF_HOME/hub` (portable mode redirects
  `HF_HOME` to `Data/huggingface` at startup — `src-tauri/src/portable.rs:44-47,
  65-69`). Lookup additionally checks the pre-v0.9.6 portable HF home
  (`hf_caches()`, `src-tauri/src/managers/model.rs:325-339`).
- Cache resolution goes through `refs/<revision>` with a fallback to
  `refs/main` so caches populated by other tools still hit
  (`hf_cached_path_in`, `src-tauri/src/managers/model.rs:341-370`).
- Partial downloads sit next to the target as `<filename>.partial` (URL/dir
  models) or hf-hub's `.sync.part` inside the cache; interrupted extractions
  use `<dir>.extracting` (`src-tauri/src/managers/model.rs:1413-1448, 2303-2327`).

### 2.2 The registry: catalog + legacy table + discovery

`ModelManager::new` builds the registry in this order
(`src-tauri/src/managers/model.rs:1149-1181`):

1. A **hardcoded legacy table** of ~16 models with `ModelSource::Url`
   (blob.handy.computer, sha256-pinned): whisper small/medium/turbo/large,
   breeze-asr, parakeet-tdt v2/v3 (ONNX dirs), moonshine base/tiny/small/medium
   streaming, sense-voice-int8, gigaam-v3, canary-180m-flash, canary-1b-v2,
   cohere-int8 (`src-tauri/src/managers/model.rs:581-1143`). Sizes on disk
   range 31 MB (moonshine tiny) to 1708 MB (cohere-int8) — see `size_mb` in
   each entry.
2. **The bundled catalog** — 69 GGUF models from the `handy-computer` HF org,
   baked into the binary via `include_str!("catalog.json")`
   (`src-tauri/src/catalog/mod.rs:118-119`; JSON at
   `src-tauri/src/catalog/catalog.json`). Counted with `python3` over the JSON
   during this session: 69 entries, each pinned to a commit revision with
   per-quant `size_bytes`/`sha256` (`QuantFile`,
   `src-tauri/src/managers/model.rs:127-136`). Per-quant disk sizes span 33 MB
   (moonshine-tiny Q8_0) to 8467 MB (Voxtral-Mini-4B F16); default quants are
   Q4_K_M-class (~130 MB–2700 MB).
3. **Discovery scans**: custom `.bin`/`.gguf` dropped into the models dir
   (`discover_custom_transcribe_models`, `src-tauri/src/managers/model.rs:1602`)
   and transcribe-cpp GGUFs already in the shared HF cache
   (`discover_hf_cache_models`, `src-tauri/src/managers/model.rs:1764`), both
   GGUF-header-probed for capabilities.

Then: migrations, `update_download_status()` (recomputes `is_downloaded` from
disk, `src-tauri/src/managers/model.rs:1401-1491`), and
`auto_select_model_if_needed()` (clears a vanished selection; picks the first
downloaded model in UI rank order — but only after onboarding is complete,
`src-tauri/src/managers/model.rs:1544-1597`).

`ModelInfo` (frontend-facing, specta-typed) carries `size_mb`, `is_downloaded`,
`is_downloading`, `partial_size`, `engine_type`, capability flags
(`src-tauri/src/managers/model.rs:60-82`). `size_mb` is computed from the
catalog file's `size_bytes / (1024*1024)` (`src-tauri/src/managers/model.rs:240`)
— **disk size, never measured RAM**.

### 2.3 Download flow

`ModelManager::download_model(model_id)` (`src-tauri/src/managers/model.rs:2222`)
routes by `ModelSource`:

- `HuggingFace` → `download_hf_model` (`src-tauri/src/managers/model.rs:1922-2179`):
  hf-hub `ApiBuilder::from_env()` (token ignored), multi-attempt schedule
  `[4,1,1,1]` concurrent streams, a 5s-tick stall watchdog, cancellation via
  `CancellationToken`; on persistent HF failure falls back to mirror URLs from
  `catalog.json` (`download_from_mirror`, `src-tauri/src/managers/model.rs:2184`),
  which writes the plain file into the models dir.
- `Url` → resumable HTTP download of `url` → `<models_dir>/<filename>.partial`,
  sha256-verified, then either extracted (`.tar.gz` → directory models, via
  `Data/handy…/models/<name>.extracting` temp dir) or renamed into place
  (`src-tauri/src/managers/model.rs:2283-2394`).
- `Local` → error, nothing to download (`src-tauri/src/managers/model.rs:2238-2240`).

Progress events emitted to the frontend: `model-download-progress`,
`model-download-complete`, `model-download-failed`, `model-verification-*`,
`model-extraction-*`, `model-download-cancelled`, `models-updated`
(`src-tauri/src/managers/model.rs:447-463, 2312, 2387, 2410`; consumed in
`src/stores/modelStore.ts:277-427`).

`delete_model` (`src-tauri/src/managers/model.rs:2420-2552`) removes HF-cache
repo dirs/blobs (hard delete from the shared cache), models-dir copies and
partials; the command wrapper first unloads + deselects the model if it was
active (`src-tauri/src/commands/models.rs:66-91`).

`get_model_path` (`src-tauri/src/managers/model.rs:2586-2661`) is the
disk-resolution used at load time: HF cache snapshot → models-dir copy →
error + `mark_model_unavailable`.

---

## 3. Exactly when a model loads

There is **no load at app startup**. `initialize_core_logic`
(`src-tauri/src/lib.rs:188-229`) creates the managers and applies accelerator
settings but never loads a model. A model loads on exactly these triggers:

1. **Recording starts** (the main dictation path): `TranscribeAction::start`
   calls `tm.initiate_model_load()` (`src-tauri/src/actions.rs:392-398`), which
   spawns a background thread that loads `settings.selected_model`
   (`src-tauri/src/managers/transcription.rs:777-804`).
2. **User switches model** (settings UI, onboarding, or tray menu):
   `switch_active_model` (`src-tauri/src/commands/models.rs:99-163`) persists
   the selection, then **eagerly** calls `transcription_manager.load_model`
   synchronously (`src-tauri/src/commands/models.rs:154`) — *unless*
   `model_unload_timeout == Immediately`, in which case it only emits a
   `selection_changed` event and skips the load (load happens on next use;
   `src-tauri/src/commands/models.rs:132-151`). Tray entries route through the
   same function on a spawned thread (`src-tauri/src/lib.rs:321-339`).
3. **History re-transcription**: `retry_history_entry_transcription` calls
   `initiate_model_load()` then `transcribe`
   (`src-tauri/src/commands/history.rs:84-90`).
4. **Headless `--transcribe-file` CLI**: `tm.load_model_with_device(&model_id,
   device_index)` with load timing (`src-tauri/src/lib.rs:535-561`), reloading
   between repeats when Immediately is set (`src-tauri/src/lib.rs:567-574`),
   and unloading at exit (`src-tauri/src/lib.rs:928-932`).

### The single choke point

Every load funnels into `TranscriptionManager::load_model_with_device`
(`src-tauri/src/managers/transcription.rs:527-774`) (`load_model` is a thin
wrapper, `src-tauri/src/managers/transcription.rs:518-520`). Its sequence:

1. `apply_accelerator_settings(&self.app_handle)` (ORT accel global;
   `src-tauri/src/managers/transcription.rs:532`, impl at `2038-2057`).
2. Emit `model-state-changed` `loading_started` (`:538-546`).
3. Look up `ModelInfo`; fail with `loading_failed` if unknown
   (`:548-563`), not downloaded (`:579-583`), or path unresolvable
   (`:585-588`).
4. **Drop the old engine first** (peak-memory avoidance): ONNX slot cleared,
   non-TranscribeCpp worker unloaded + waited, `current_model_id = None`
   (`:590-602`).
5. Build the engine per type (`:606-744`):
   - TranscribeCpp: pick backend/device from settings or explicit
     `device_index` (`:615-632`), call `self.engine.load(LoadSpec { path,
     backend, device })` (`:634-645`) — this (re)spawns the worker process and
     loads the model inside it; returns `LoadedInfo` (arch, variant, backend,
     device, on_gpu, capabilities — `src-tauri/src/engine_supervisor/protocol.rs:144-156`).
     Then reconciles registry capabilities with runtime truth via
     `ModelManager::set_runtime_capabilities` (`:647-657`, impl
     `src-tauri/src/managers/model.rs:1306-1328`).
   - ONNX types: `ParakeetModel::load` / `MoonshineModel::load` /
     `StreamingModel::load` / `SenseVoiceModel::load` / `GigaAMModel::load` /
     `CanaryModel::load` / `CohereModel::load` in-process (`:675-743`).
6. Store the engine, set `current_model_id`, `touch_activity()`
   (`:746-754`), emit `loading_completed` (`:756-765`).

Concurrency gate: `try_start_loading()` — an `is_loading` mutex + condvar with
an RAII `LoadingGuard` (`src-tauri/src/managers/transcription.rs:196-217, 435-445`).
`switch_active_model` claims it (double-click protection,
`src-tauri/src/commands/models.rs:103-108`); `initiate_model_load` checks it
and returns early if a load is already running
(`src-tauri/src/managers/transcription.rs:778-788`). `transcribe()` and the
stream worker **wait** on the condvar until any in-flight load finishes
(`src-tauri/src/managers/transcription.rs:1152-1161, 874-879`).

A stale-flag reload path also exists: accelerator/GPU-device changes call
`reload_model_on_next_use()` (flag store, `src-tauri/src/managers/transcription.rs:389-391`),
honored by `initiate_model_load` (`:783-795`); invoked from
`save_accelerator_and_reload_next_use` (`src-tauri/src/shortcut/mod.rs:1410-1417`).

---

## 4. Staying resident + unloading: the ModelUnloadTimeout system

### 4.1 The setting

```rust
pub enum ModelUnloadTimeout { Never, Immediately, Min2, Min5 /*default*/, Min10, Min15, Hour1, Sec15 /*debug*/ }
```
`src-tauri/src/settings.rs:134-146` (`#[serde(rename_all = "snake_case")]`,
`#[default]` = `Min5`). Stored as `AppSettings.model_unload_timeout`
(`src-tauri/src/settings.rs:443`), default applied in `get_default_settings`
(`src-tauri/src/settings.rs:958`), persisted through tauri-plugin-store in
`settings_store.json` (`SETTINGS_STORE_PATH`,
`src-tauri/src/settings.rs:878`; `get_settings` at `:1037`).

`to_seconds()`: Never→`None`, Immediately→`Some(0)`, Sec15→`Some(15)`,
Min2/5/10/15→120/300/600/900, Hour1→3600 (`src-tauri/src/settings.rs:253-261`).
Note `to_minutes()` maps Sec15→`Some(0)` with a "handled separately" comment
(`src-tauri/src/settings.rs:249`) — a latent trap for any new consumer of
`to_minutes()` (no current callers outside settings.rs; verified by grep).

### 4.2 The idle watcher thread (resident→unload)

Spawned in `TranscriptionManager::new`
(`src-tauri/src/managers/transcription.rs:298-367`), a plain `std::thread` that
loops forever:

1. `thread::sleep(10s)` (`:306`) — so the effective unload deadline has up to
   ~10 s of latency past the configured limit.
2. Read fresh settings each tick (`settings.model_unload_timeout`, `:313-314`)
   — changing the setting takes effect on the next tick, no restart.
3. **Skip `Immediately`** here on purpose — it would unload mid-recording;
   that variant is handled per-transcription instead (`:317-321`).
4. **While recording, refresh the timer** (`AudioRecordingManager::
   is_recording()` → `touch_activity()` → `continue`) so the model is never
   unloaded mid-session (`:324-331`).
5. If `idle_ms > limit_ms` and `is_model_loaded()` → `unload_model()` with
   timing logs (`:333-361`).

`touch_activity()` stores `last_activity = now_ms`
(`src-tauri/src/managers/transcription.rs:493-503`). It is touched at:
load completion (`:754`), every `transcribe()` call (`:1137`), every streamed
audio frame fed to the engine (`:989`), and stream start (`:980`).

Watcher shutdown: `Drop for TranscriptionManager` sets `shutdown_signal` and
joins the thread, but only on the last `Arc` clone
(`src-tauri/src/managers/transcription.rs:2500-2533`).

### 4.3 Unload triggers (complete list)

- **Idle timeout** (above).
- **`Immediately` mode**: `maybe_unload_immediately(context)` — called after
  every batch transcription (`src-tauri/src/managers/transcription.rs:1248`),
  after streaming finalize (`:1099`), on empty audio (`:1146`), from the
  recording pipeline's `FinishGuard` drop (`src-tauri/src/actions.rs:36-48`),
  and on cancellation (`src-tauri/src/utils.rs:110`). It uses the
  non-blocking `request_unload` (`src-tauri/src/managers/transcription.rs:508-516`).
- **Tray menu "Unload model"** (enabled only when a model is loaded,
  `src-tauri/src/tray.rs:549-556`): handler at `src-tauri/src/lib.rs:303-311`
  calls `request_unload()`.
- **Frontend command** `unload_model_manually` → `request_unload()`
  (`src-tauri/src/commands/transcription.rs:33-40`).
- **Deleting the active model**: waits for the worker to exit first
  (`unload_model()` in `spawn_blocking`,
  `src-tauri/src/commands/models.rs:72-86`).
- **Headless exit** (`src-tauri/src/lib.rs:928-932`).
- **Engine-dropped reconciliation**: if the worker crashed/quit on its own,
  `forget_model_if_engine_dropped` clears `current_model_id` and emits an
  `unloaded` event with the error (`src-tauri/src/managers/transcription.rs:405-429`).
  ONNX engine panics are caught with `catch_unwind` and the engine is dropped
  (not returned to the slot) — effectively an unload (`:1377, 1466-1498`).

### 4.4 Unload mechanics

`begin_unload` (`src-tauri/src/managers/transcription.rs:471-491`) does all
state changes synchronously on the caller's thread — `engine.unload()` (which
clears the supervisor's `loaded` snapshot *immediately* and queues the worker
exit ahead of anything requested later, `src-tauri/src/engine_supervisor/supervisor.rs:370-385`),
drops the ONNX engine (freeing its resources), nulls `current_model_id`, and
emits `model-state-changed` `unloaded`. Only the worker's process exit is
asynchronous: `Unloading::wait()` (`src-tauri/src/engine_supervisor/supervisor.rs:481-489`)
blocks until the child is gone (worker gets `SHUTDOWN_GRACE`, then killed and
reaped — `src-tauri/src/engine_supervisor/supervisor.rs:1085-1088, 1290-1305`).
`unload_model()` waits; `request_unload()` doesn't
(`src-tauri/src/managers/transcription.rs:449-465`).

**This is why unload frees memory**: for transcribe-cpp models the whole worker
process dies, returning *"all of its CPU and GPU memory to the OS"*
(`src-tauri/src/engine_supervisor/mod.rs:12-15`); for ONNX models the engine
object is dropped in-process.

---

## 5. Querying the currently loaded model

### 5.1 From Rust

- `TranscriptionManager::is_model_loaded()` — `self.engine.loaded().is_some()
  || self.lock_onnx().is_some()` (`src-tauri/src/managers/transcription.rs:380-384`).
  Snapshot, non-blocking ("A transcribe-cpp model stays loaded while it is
  busy, so a batch run or stream in progress never reads as 'unloaded'").
- `TranscriptionManager::get_current_model() -> Option<String>` — the
  `current_model_id` mutex (`src-tauri/src/managers/transcription.rs:806-809`).
  `None` when nothing is loaded. Note this is the *loaded* id, which can
  differ from `settings.selected_model` (e.g. Immediately mode between loads,
  or a headless `--model` override).
- `TranscriptionManager::current_backend() -> Option<String>` — bound backend
  string for transcribe-cpp, `"onnx"` for ONNX, `None` if nothing loaded
  (`src-tauri/src/managers/transcription.rs:811-821`).
- `EngineSupervisor::loaded() -> Option<LoadedInfo>` — full snapshot incl.
  arch/variant/backend/device/on_gpu/capabilities
  (`src-tauri/src/engine_supervisor/supervisor.rs:396-399`;
  `LoadedInfo` at `src-tauri/src/engine_supervisor/protocol.rs:144-156`).
- For contrast, `settings.selected_model` is the *persisted preference*
  (default `""`, `src-tauri/src/settings.rs:414-415, 535-537`) — populated by
  `switch_active_model` or `auto_select_model_if_needed`.

### 5.2 From the frontend (Tauri commands, specta-typed in `src/bindings.ts`)

| Command | Rust | Binding | Returns |
|---|---|---|---|
| `get_model_load_status` | `commands/transcription.rs:22-31` | `bindings.ts:846` | `{ is_loaded: bool, current_model: string \| null }` (type at `bindings.ts:1063`) |
| `get_transcription_model_status` | `commands/models.rs:183-189` | `bindings.ts:699` | `Option<String>` — loaded model id |
| `get_current_model` | `commands/models.rs:176-181` | `bindings.ts:691` | `settings.selected_model` (preference, **not** loaded state) |
| `unload_model_manually` | `commands/transcription.rs:33-40` | `bindings.ts:854` | unloads (async) |
| `set_model_unload_timeout` | `commands/transcription.rs:14-20` | `bindings.ts:843` | persists the setting |
| `is_model_loading` | `commands/models.rs:191-199` | `bindings.ts:707` | ⚠ misnomer — returns `current_model.is_none()` (true when *nothing is loaded*), not load-in-progress |

**Events** (string payload event `model-state-changed`, `ModelStateEvent {
event_type, model_id, model_name, error }`,
`src-tauri/src/managers/transcription.rs:53-59`): `loading_started`,
`loading_completed`, `loading_failed`, `unloaded`, `selection_changed`
(emitted from the load/unload paths cited above). Frontend consumers:

- `ModelSelector.tsx:50-69` polls `getTranscriptionModelStatus` when
  `currentModel` changes and maps `loaded==selected → "ready"`, else
  `"unloaded"`; `ModelSelector.tsx:71-98` switches local status on every
  `model-state-changed`.
- `modelStore.ts:419-422` reloads models + current selection on
  `model-state-changed`; `settingsStore.ts:654` similarly refreshes settings.
- `App.tsx:197-215` toasts on `loading_failed`.
- The tray rebuilds its menu on `model-state-changed`
  (`src-tauri/src/lib.rs:355-359`); the tray's model submenu checkmarks the
  *settings-selected* model and enables "Unload model" only while loaded
  (`src-tauri/src/tray.rs:540-566`).

Unused today: `get_model_load_status` is exposed in bindings but **no frontend
component calls it** (verified by grep over `src/` — only bindings.ts matches).
The ModelSelector uses `getTranscriptionModelStatus` instead.

---

## 6. RAM of a loaded model: what exists and what doesn't

**Nothing in the codebase measures or displays model RAM / process RSS.**
Verified by grepping the whole `src-tauri/src` tree for
`sysinfo|rss|resident|memory_usage|used_memory|footprint|task_vm|mach_task|
proc_pid|memory_info` — the only hits are comments (the word "resident" in
`audio_toolkit/audio/recorder.rs:63,125` about engines, and `memory.rs:14`
describing RSS growth of the app process) and `Cargo.toml` has **no** sysinfo
or similar crate. Likewise no frontend code displays RAM (grep for
`memory|vram` in `src/` hits only `AccelerationSelector.tsx` and a comment in
`LiveLogViewer.tsx`).

What *does* exist:

1. **Disk size as the only size signal**: `ModelInfo.size_mb`
   (`src-tauri/src/managers/model.rs:67, 240`) from catalog `size_bytes`.
   Catalog default quants: ~33 MB (moonshine-tiny Q8_0) up to ~2.7 GB
   (Voxtral-Mini Q4_K_M); max listed quant 8467 MB (Voxtral F16). Legacy table
   entries: 31–1708 MB. (Enumerated with python3 over
   `src-tauri/src/catalog/catalog.json` this session.) For GGUF engines
   loaded via gguf mmap the resident set is typically file-size-plus-compute-
   buffers, but **that is engine (transcribe-cpp/whisper.cpp) behavior, not
   anything this repo states or measures** — treat as unverified inference.
2. **GPU VRAM capacity, not usage**: `DeviceInfo.memory_total`
   (`src-tauri/src/engine_supervisor/protocol.rs:99,113`) →
   `GpuDeviceOption.total_vram_mb`
   (`src-tauri/src/managers/transcription.rs:2059-2064, 2097-2113`), displayed
   as "8.0 GB" labels in `src/components/settings/AccelerationSelector.tsx:76-82`.
   This is the device's total VRAM reported at device-listing time.
3. **Allocator hygiene (Linux/glibc only)**: `src-tauri/src/memory.rs` —
   `init_allocator()` pins `M_MMAP_THRESHOLD` to 128 KiB (called at the top of
   `run()` and in the worker's `run()`,
   `src-tauri/src/engine_supervisor/worker.rs:35`), and `trim_freed_memory()`
   calls `malloc_trim(0)` after each transcription pipeline
   (`src-tauri/src/actions.rs:43-47`, worker after each run/stream at
   `worker.rs:136,154`). The module doc quantifies the transient-buffer
   problem it fixes (~15 MB retained per 2-min dictation → ~0.5 MB; issue
   #1792). No-ops on macOS/Windows/musl (`src-tauri/src/memory.rs:29-39, 47-57`).
   This is about *transcription buffers*, not model weights.
4. **Timing, not bytes**: load and unload durations are logged
   (`src-tauri/src/managers/transcription.rs:342-358, 450-456, 767-772`) —
   the only "cost" telemetry that exists.

So: "how much RAM does a loaded model occupy" is currently **unanswerable from
the app** — a feature would have to measure it (see §8).

---

## 7. Where a memory-pressure check could hook in before a load

Ranked seams, most natural first:

1. **`TranscriptionManager::load_model_with_device`**
   (`src-tauri/src/managers/transcription.rs:527`) — the *single* function all
   four load triggers funnel through. A pressure check belongs right after
   `apply_accelerator_settings` (`:532`) / before the old-engine drop
   (`:590`). Threading context of callers: `initiate_model_load`'s background
   thread (`:790`), a `std::thread` for tray switches (`src-tauri/src/lib.rs:328`),
   the async-runtime thread for the `set_active_model` command (blocking it —
   it already blocks on the synchronous load,
   `src-tauri/src/commands/models.rs:154`), and the headless thread
   (`src-tauri/src/lib.rs:556-561`). A blocking OS query here is acceptable
   but note it runs on the tauri async worker for command-triggered loads.
2. **`switch_active_model`** (`src-tauri/src/commands/models.rs:99-163`) —
   policy decision point: under pressure it could skip the *eager* load the
   same way the `Immediately` branch already does (`:132-151`), falling back to
   on-demand.
3. **`initiate_model_load`** (`src-tauri/src/managers/transcription.rs:777-804`)
   — gate for on-demand loads at recording start / history retry.
4. **The idle watcher loop** (`src-tauri/src/managers/transcription.rs:303-367`)
   — already a 10-second poll that reads settings and calls
   `is_model_loaded()`/`unload_model()`; the natural home for a *reactive*
   pressure-driven unload (symmetric to the time-based one).
5. **`crate::memory`** (`src-tauri/src/memory.rs`) — the obvious module to
   grow a `system_memory()` / `worker_rss(pid)` probe. No sysinfo crate today
   (`src-tauri/Cargo.toml` has none — verified by grep), so either add one or
   write the three platform probes (macOS `sysctl`/`mach`; Linux
   `/proc/meminfo` + `/proc/<pid>/status`; Windows `GlobalMemoryStatusEx`).
6. **Worker-process measurement**: the worker's pid is already tracked
   (`Control { pid, child, stdin }`,
   `src-tauri/src/engine_supervisor/supervisor.rs:1057-1066`), so the parent
   can poll the child's RSS out-of-band; alternatively extend the framed
   protocol (`Command` enum, `src-tauri/src/engine_supervisor/supervisor.rs:177-205`;
   `Response` enum, `src-tauri/src/engine_supervisor/protocol.rs:69-88`) with a
   self-report message, and/or add a memory field to `LoadedInfo`
   (`protocol.rs:144-156`). Remember ONNX models live in the *app* process —
   a complete picture needs both.
7. **Frontend surfacing**: `ModelLoadStatus`
   (`src-tauri/src/commands/transcription.rs:8-12`) is an obvious vehicle
   (it's currently unused by the UI), plus `ModelStateEvent`
   (`src-tauri/src/managers/transcription.rs:53-59`) / the
  `model-state-changed` listeners in `ModelSelector.tsx:71-98` and
  `modelStore.ts:419-422` already refresh on every lifecycle change. Specta
  bindings regenerate into `src/bindings.ts` (see research-build.md).

Handy primitives a pressure feature can reuse: `try_start_loading` /
`LoadingGuard` (exclusive load slot), `request_unload` (non-blocking unload),
`engine.loaded()` (instant snapshot), and `ModelInfo.size_mb` as a
pressure *forecast* (disk-size proxy for what a load will cost).

---

## 8. Risks, quirks, and traps

- **`bindings.ts` vs serde wire-format mismatch for `ModelUnloadTimeout`.**
  The generated TS type says `"min_2" | "min_5" | "min_10" | "min_15" |
  "hour_1" | "sec_15"` (`src/bindings.ts:1088`) but serde's `snake_case` for
  `Min2` is `"min2"` — proven by the frozen v0.9 store test fixture, which
  parses `"model_unload_timeout": "min5"` strictly
  (`src-tauri/src/settings.rs:1383`, test at `:1333-1334`; not executed by me,
  read from source). The shipped React dropdown sends `"min2"`-style values
  and casts them to the TS type (`src/components/settings/ModelUnloadTimeout.tsx:30-46`),
  i.e. it works at runtime and the *type annotation* is what's wrong. Any new
  frontend code that trusts the literal `"min_5"` from bindings will send a
  string serde rejects → the command errors. Verify the accepted wire strings
  empirically before sending new values.
- **`is_model_loading` is misnamed**: it returns `current_model.is_none()`
  (`src-tauri/src/commands/models.rs:191-199`) — `true` means "no model
  loaded", nothing about a load in progress. Real in-progress signal is the
  `is_loading` mutex/condvar (private) or the `loading_started` event.
- **Selected vs loaded are different things** and four commands expose them
  inconsistently (`get_current_model` = preference; `get_transcription_model_status`
  = loaded id; `get_model_load_status` = both, unused). In `Immediately` mode
  they diverge almost always.
- **Unload latency**: the idle watcher polls every 10 s
  (`src-tauri/src/managers/transcription.rs:306`); actual unload happens up to
  one poll past the limit. `Immediately` unloads right after each
  transcription/stream/cancel instead.
- **`to_minutes()` maps `Sec15 → Some(0)`** (`src-tauri/src/settings.rs:249`)
  — only `to_seconds()` handles it (15). New code must use `to_seconds()`.
- **Two process homes for model RAM** (worker vs in-process ONNX, §1) — any
  memory measurement or pressure reaction must handle both, or it will be
  wrong for the legacy ONNX models (parakeet dirs, moonshine, sensevoice,
  canary, cohere, gigaam).
- **Eager load on switch is synchronous** and holds the exclusive loading
  slot; under memory pressure it is also the moment two models could
  transiently coexist — except the code deliberately drops the old engine
  first (`src-tauri/src/managers/transcription.rs:590-598`), so peak = max
  (old, new), not sum. A pre-load pressure check must run *before* that drop
  if it wants to consider the outgoing model.
- **Worker crash ≠ clean state**: after engine-dropped reconciliation the
  loaded id clears and the next use reloads
  (`src-tauri/src/managers/transcription.rs:405-429`); a pressure feature
  keying off `is_model_loaded()` right after a crash may see `false` while the
  user still expects a model.
- **GGUF disk size ≠ RAM**: `size_mb` is the only size data in-app; actual
  residency depends on transcribe-cpp/gguf mmap behavior (not in this repo).
  Don't present `size_mb` as measured RAM without labeling it an estimate.
- **Portable mode moves everything** (`src-tauri/src/portable.rs`): paths and
  the HF cache differ; anything that scans disk for models must use
  `portable::app_data_dir` / `hf_caches()`, not raw env assumptions.

---

## 9. Quick reference: file map

| Concern | File:lines |
|---|---|
| Registry struct + dir | `src-tauri/src/managers/model.rs:542-562` |
| Legacy model table | `src-tauri/src/managers/model.rs:581-1143` |
| Catalog seeding / rescan | `src-tauri/src/managers/model.rs:1212-1286`; `src-tauri/src/catalog/mod.rs` |
| Downloads (HF/URL/mirror) | `src-tauri/src/managers/model.rs:1922-2418` |
| Path resolution | `src-tauri/src/managers/model.rs:2586-2661` |
| Load choke point | `src-tauri/src/managers/transcription.rs:527-774` |
| On-demand loader | `src-tauri/src/managers/transcription.rs:777-804` |
| Idle watcher | `src-tauri/src/managers/transcription.rs:298-367` |
| Unload paths | `src-tauri/src/managers/transcription.rs:449-516`; `src-tauri/src/engine_supervisor/supervisor.rs:370-389` |
| Timeout enum | `src-tauri/src/settings.rs:134-146, 239-261` |
| Status commands | `src-tauri/src/commands/transcription.rs`; `src-tauri/src/commands/models.rs` |
| Frontend store / selector | `src/stores/modelStore.ts`; `src/components/model-selector/ModelSelector.tsx` |
| Allocator hygiene | `src-tauri/src/memory.rs` |
| Bindings | `src/bindings.ts` (specta-generated) |
