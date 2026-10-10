# Cycle 2 flagship: the post-processing overhaul (operator brief, 2026-10-10)

Operator requirements, verbatim intent, mapped to the codebase as it stands on main after v1.2.5 + PR #8 (local_llm module). This brief is the primary input for the Cycle 2 workflow's architect.

## What the operator asked for

1. A huge amount of Hugging Face models for post-processing: more configurations, more flexibility, more visibility. Change models at will, exactly like the voice models.
2. Post-processing is a black box today ("logs are absolute shit"): it must be visible whether it is working, not working, and WHY it is not working.
3. Stability everywhere: multiple new models, released models, hot-swapping.
4. A prompt-template library per language and register: English professional, English casual, Hinglish, Hindi, and more; improve the existing default prompt.

## Current state (verified on main)

- Local engine: exactly ONE pinned model (local_llm/mod.rs: `LOCAL_LLM_MODEL_NAME = "Qwen3 0.6B (post-process)"`), llama-cpp-2 worker (worker.rs, 343 lines) with a memory-aware planner (planner.rs, 1122 lines) that already understands swap-in-progress and refusals. No LLM catalog, no downloads, no per-model settings.
- Cloud path: 9 hardcoded providers in settings (post_process_providers: openai, zai, openrouter, anthropic, groq, cerebras, apple_intelligence, bedrock-mantle, custom), each with a models_endpoint that is NEVER called; model selection is a free-text string per provider (post_process_models dict, mostly empty); ONE built-in prompt (default_improve_transcriptions) with ${output} substitution; execution is actions.rs `post_process_transcription` with a JSON output schema; a SkipReason enum and a skipToastDedupe frontend helper exist (PR #8), so some skip surfacing is already in place.
- Binding: `transcribe_with_post_process` is a separate hotkey (option+shift+space) from plain transcribe.

## Target architecture (design direction for the Cycle 2 architect)

### A. LLM model catalog, mirroring the voice ModelManager

- New curated catalog of GGUF LLMs for text cleanup (seed: Qwen3 0.6B/1.7B, Llama 3.2 1B/3B, Gemma 3 4B and the Phi mini class, Q4_K_M default quants, from reputable GGUF publishers such as bartowski/unsloth), plus user-added arbitrary HF repos exactly like voice models.
- Downloads through the existing hf-hub plumbing with progress UI; storage under app-data/llm-models (separate from ASR models); RAM forecasts from the existing local_llm planner extended per-model (context length + quant), integrated with the memory gate so post-process never evicts the active ASR model without consent.
- Settings UI parity with the Models tab: cards with size/quant/context/languages, download/delete, active badge, and the same search. Tray submenu: active post-process model.
- The planner's swap semantics become "switch model any time, even mid-session": new dictations use the new model; an in-flight post-process finishes on the old one.

### B. Cloud provider models made real

- Call each provider's models_endpoint on demand (button + on-open refresh, cached, error-surfaced) and populate a dropdown; keep free-text override for endpoints that lie.
- A Test connection button per provider (tiny completion request) with a clear verdict: auth ok, latency, model reachable, and the failure class when not.

### C. Observability: no more black box (the operator's loudest ask)

- A structured post-process lifecycle, logged with a greppable prefix (pattern: `pp:` lines, like the successful `cmd-mode:` lines): requested (binding, provider/model id, prompt id, template language), engine phase (model load ms / cache hit / swap wait), generation (tokens, ms, retries), outcome (applied | skipped + SkipReason | failed + failure class enum: auth, network, timeout, context-length, output-invalid, oom, cancelled), and the before/after diff summary (chars in/out, changed ratio).
- Same lifecycle as Tauri events driving: an overlay status chip while processing (with elapsed), a toast on terminal states (skip and failure today reach toasts only partially; keep dedupe), and a per-run record in history (which model + prompt processed each entry; visible in the History UI row detail).
- A Debug-tab "Post-process runs" table: last N runs with provider/model, prompt, latency, outcome, failure class; one click copies the full log slice for that run.
- Failure NEVER loses the transcript: on any failure, paste the raw transcript and notify visibly (overlay toast + notification), exactly like the voice fallback UX Wave 1 builds.

### D. Prompt-template library

- Named, editable templates with metadata: language (en, hi, hi-Latn, and more), register (professional, casual, technical, minimal), description; variables documented ({{output}}, optional {{context_app}}). Seed set to ship built-in (all translated descriptions): English Professional, English Casual, English Technical (code-friendly), English Minimal (punctuation only), Hindi (Devanagari), Hinglish (Roman script, keep Hinglish flavor), Marathi, Tamil, Spanish, French, German, Japanese, Chinese (simplified). Keep-language enforcement stays a hard rule in every template.
- Template manager UI in the Post-processing tab: list, edit, duplicate, per-template "test on my last transcript" button (runs the active provider/model on the most recent history entry and shows before/after), and a per-binding default (plain transcribe binding untouched; the post-process binding gets a selectable template, cycled by a tray submenu or an assignable hotkey).
- Prompt version stamped into each run record so history explains itself.

### E. Stability requirements

- Timeouts (configurable, default sane per provider class), bounded retries with backoff for network-class failures only, cancellation (Escape binding already exists; wire it), idle unload for local models with the same 30s-style policy as ASR, and output validation that falls back to raw paste on anything malformed (never paste garbage from a hallucinating model: validate non-empty, reasonable length ratio, language sanity check).

## Non-goals / guardrails for Cycle 2

- Do not break the v1.3.0 polish work landing first; build on that branch state.
- Cloud API keys stay in the settings store as today (no new secret storage surface without an explicit operator decision).
- The local engine stays the recommended default (privacy-first product stance); cloud is the flexibility option.
