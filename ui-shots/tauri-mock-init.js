/*
 * Browser-side Tauri IPC mock for the VoxBar screenshot rig.
 *
 * Injected by ui-shots/shoot.mjs via context.addInitScript({ path }) so it
 * runs before any app module. Every Tauri v2 API call in the app funnels
 * through window.__TAURI_INTERNALS__.invoke (verified against
 * node_modules/@tauri-apps/api/core.js:202 and event.js), plus the
 * synchronous window.__TAURI_OS_PLUGIN_INTERNALS__ injected by plugin-os.
 * Mocking those two objects covers core invoke, the event bus, all plugins
 * (os, app, updater, macos-permissions, dialog, opener, fs, autostart) and
 * the tauri-specta generated bindings in src/bindings.ts.
 *
 * Scenario selection: ?voxshot=<name> applies a preset on top of the
 * faithful default-settings fixture (mirrors get_default_settings() in
 * src-tauri/src/settings.rs at macOS defaults). Runtime control:
 * window.__voxshot.emit(event, payload) dispatches a Tauri event to every
 * registered listener, and window.__voxshot.state is the mutable backing
 * store. Unknown commands resolve null and are logged in
 * window.__voxshot.calls.unknown so the rig can report coverage gaps.
 */
(() => {
  if (window.__TAURI_INTERNALS__) return;

  const params = new URLSearchParams(window.location.search);
  const scenarioName = params.get("voxshot") || "settings-default";
  const APP_VERSION = "1.2.5";

  const LOCAL_LLM_MODEL_ID = "Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf";
  const PARAKEET_ID =
    "handy-computer/parakeet-unified-en-0.6b-gguf/parakeet-unified-en-0.6b-Q8_0.gguf";
  const CANARY_ID =
    "handy-computer/canary-180m-flash-gguf/canary-180m-flash-Q8_0.gguf";

  const SHORT_ID = (repoAndFile) =>
    repoAndFile.split("/").slice(0, 2).join("/");

  // Default command matrix, faithful to default_command_matrix() in
  // src-tauri/src/audio_toolkit/command_matrix.rs (English defaults).
  const DEFAULT_COMMAND_MATRIX = [
    { command: "period", phrases: ["period", "full stop"] },
    { command: "comma", phrases: ["comma"] },
    { command: "questionMark", phrases: ["question mark"] },
    {
      command: "exclamation",
      phrases: ["exclamation mark", "exclamation point"],
    },
    { command: "colon", phrases: ["colon"] },
    { command: "semicolon", phrases: ["semicolon"] },
    { command: "dash", phrases: ["dash"] },
    { command: "newLine", phrases: ["new line"] },
    { command: "newParagraph", phrases: ["new paragraph"] },
    { command: "atSign", phrases: ["at sign"] },
    { command: "hash", phrases: ["hash", "hash sign"] },
    { command: "dollarSign", phrases: ["dollar sign"] },
    { command: "percent", phrases: ["percent", "percent sign"] },
    { command: "star", phrases: ["star", "asterisk"] },
    { command: "ampersand", phrases: ["ampersand"] },
    { command: "caret", phrases: ["caret"] },
    { command: "openParen", phrases: ["open paren", "open parenthesis"] },
    { command: "closeParen", phrases: ["close paren", "close parenthesis"] },
    {
      command: "openBracket",
      phrases: ["open bracket", "open square bracket"],
    },
    {
      command: "closeBracket",
      phrases: ["close bracket", "close square bracket"],
    },
    { command: "openBrace", phrases: ["open brace", "open curly brace"] },
    { command: "closeBrace", phrases: ["close brace", "close curly brace"] },
    { command: "slash", phrases: ["slash", "forward slash"] },
    { command: "backslash", phrases: ["backslash"] },
    { command: "pipe", phrases: ["pipe", "vertical bar"] },
    {
      command: "deleteWord",
      phrases: ["delete word", "scratch that", "delete that", "remove that"],
    },
    { command: "deleteLine", phrases: ["delete line"] },
    {
      command: "clearAll",
      phrases: ["delete everything", "scratch everything"],
    },
    { command: "undo", phrases: ["undo"] },
    { command: "paste", phrases: ["paste"] },
  ];

  // Faithful mirror of get_default_settings() (src-tauri/src/settings.rs)
  // at macOS defaults, plus the post-process provider list.
  const defaultSettings = () => ({
    settings_schema_version: 2,
    bindings: {
      transcribe: {
        id: "transcribe",
        name: "Transcribe",
        description: "Converts your speech into text.",
        default_binding: "option+space",
        current_binding: "option+space",
      },
      transcribe_with_post_process: {
        id: "transcribe_with_post_process",
        name: "Transcribe with Post-Processing",
        description:
          "Converts your speech into text and applies AI post-processing.",
        default_binding: "option+shift+space",
        current_binding: "option+shift+space",
      },
      cancel: {
        id: "cancel",
        name: "Cancel",
        description: "Cancels the current recording.",
        default_binding: "escape",
        current_binding: "escape",
      },
      delete_last_word: {
        id: "delete_last_word",
        name: "Delete Last Word",
        description:
          "Removes the last word from the live dictation transcript while a recording session is active; does nothing otherwise. Unbound by default.",
        default_binding: "",
        current_binding: "",
      },
      undo: {
        id: "undo",
        name: "Undo",
        description:
          "While a dictation is live: clears it (start over). Does nothing otherwise. Unbound by default.",
        default_binding: "",
        current_binding: "",
      },
      transcribe_commands: {
        id: "transcribe_commands",
        name: "Command Mode",
        description:
          "Hold during a live dictation to switch it into command interpretation. Unbound by default.",
        default_binding: "",
        current_binding: "",
      },
    },
    shortcut_activation: "hold_or_toggle",
    hold_threshold_ms: 300,
    audio_feedback: false,
    audio_feedback_volume: 1.0,
    sound_theme: "marimba",
    start_hidden: false,
    autostart_enabled: false,
    update_checks_enabled: true,
    update_policy: "ask",
    show_whats_new_on_update: true,
    whats_new_last_seen_version: APP_VERSION,
    selected_model: "",
    onboarding_completed: false,
    always_on_microphone: false,
    selected_microphone: null,
    selected_channel: null,
    clamshell_microphone: null,
    selected_output_device: null,
    translate_to_english: false,
    selected_language: "auto",
    overlay_position: "bottom",
    debug_mode: false,
    log_level: "debug",
    custom_words: ["VoxBar"],
    model_unload_timeout: "min_5",
    memory_pressure_guard: true,
    memory_gate_headroom_mb: 0,
    auto_fallback: true,
    menu_bar_model_title: true,
    word_correction_threshold: 0.18,
    history_limit: 5,
    show_history_model: true,
    recording_retention_period: "preserve_limit",
    paste_method: "ctrl_v",
    clipboard_handling: "dont_modify",
    auto_submit: false,
    auto_submit_key: "enter",
    post_process_enabled: false,
    post_process_timeout_secs: 60,
    post_process_provider_id: "local",
    post_process_local_default_migrated: true,
    post_process_providers: [
      {
        id: "local",
        label: "Local (on-device)",
        base_url: "voxbar://local",
        allow_base_url_edit: false,
        models_endpoint: null,
        supports_structured_output: true,
      },
      {
        id: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        allow_base_url_edit: false,
        models_endpoint: "/models",
        supports_structured_output: true,
      },
      {
        id: "zai",
        label: "Z.AI",
        base_url: "https://api.z.ai/api/paas/v4",
        allow_base_url_edit: false,
        models_endpoint: "/models",
        supports_structured_output: true,
      },
      {
        id: "openrouter",
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        allow_base_url_edit: false,
        models_endpoint: "/models",
        supports_structured_output: true,
      },
    ],
    post_process_api_keys: {
      local: "",
      openai: "",
      zai: "",
      openrouter: "",
    },
    post_process_models: {
      local: LOCAL_LLM_MODEL_ID,
      openai: "gpt-4o-mini",
      zai: "glm-4.7-flash",
      openrouter: "",
    },
    post_process_prompts: [
      {
        id: "default_improve_transcriptions",
        name: "Improve Transcriptions",
        prompt:
          "<transcript>\n${output}\n</transcript>\n\nThe above is a transcript generated by a speech-to-text model. Clean it by fixing spelling, capitalization, and punctuation errors. Return only the cleaned text.",
      },
    ],
    post_process_selected_prompt_id: "default_improve_transcriptions",
    mute_while_recording: false,
    append_trailing_space: false,
    app_language: "en",
    theme: "system",
    accent_color: "pink",
    experimental_enabled: false,
    lazy_stream_close: false,
    keyboard_implementation: "handy_keys",
    show_tray_icon: true,
    paste_delay_ms: 60,
    paste_delay_after_ms: 60,
    reliable_paste: false,
    typing_tool: "auto",
    external_script_path: null,
    filler_word_removal_enabled: true,
    custom_filler_words: null,
    command_phrases: null,
    spoken_punctuation: true,
    auto_interpret_commands: true,
    terminal_punctuation: true,
    voice_deletion_commands: true,
    preview_before_paste: true,
    delete_last_word_enabled: true,
    undo_enabled: true,
    command_mode_enabled: true,
    chinese_script: "as_transcribed",
    number_format: "digits",
    transcribe_accelerator: "auto",
    ort_accelerator: "auto",
    transcribe_gpu_device: null,
    extra_recording_buffer_ms: 0,
    streaming_release_tail_ms: 200,
    vad_enabled: true,
    vad_backend: "silero",
    overlay_style: "live",
  });

  // Realistic model catalog (names/ranks from src-tauri/src/catalog/catalog.json).
  const modelCatalog = () => {
    const mk = (over) =>
      Object.assign(
        {
          filename: "",
          source: { HuggingFace: { repo_id: "", revision: "main" } },
          is_downloaded: false,
          is_downloading: false,
          partial_size: 0,
          is_directory: false,
          engine_type: "TranscribeCpp",
          supports_translation: false,
          supported_languages: ["en"],
          supports_language_selection: false,
          is_custom: false,
          supports_streaming: false,
          supports_language_detection: false,
        },
        over,
      );
    const hf = (repoId, file) => ({
      HuggingFace: { repo_id: repoId, revision: "main" },
      file,
    });
    return [
      mk({
        id: PARAKEET_ID,
        name: "Parakeet Unified EN 0.6B",
        description: "Fast English model. Great daily driver for dictation.",
        filename: "parakeet-unified-en-0.6b-Q8_0.gguf",
        source: hf(SHORT_ID(PARAKEET_ID)),
        size_mb: 644,
        is_downloaded: true,
        accuracy_score: 0.9,
        speed_score: 0.79,
        is_recommended: true,
      }),
      mk({
        id: "handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
        name: "Nemotron Streaming 3.5",
        description:
          "Multilingual streaming model with live text in the overlay.",
        filename: "nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
        source: hf("handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf"),
        size_mb: 648,
        accuracy_score: 0.82,
        speed_score: 0.84,
        is_recommended: true,
        supports_streaming: true,
        supported_languages: ["en", "es", "fr", "de", "it", "pt", "nl", "hi"],
      }),
      mk({
        id: CANARY_ID,
        name: "Canary 180M Flash",
        description: "Tiny and instant. Good for short commands.",
        filename: "canary-180m-flash-Q8_0.gguf",
        source: hf(SHORT_ID(CANARY_ID)),
        size_mb: 228,
        is_downloaded: true,
        accuracy_score: 0.88,
        speed_score: 0.98,
        is_recommended: true,
        supported_languages: ["en", "de", "es", "fr"],
      }),
      mk({
        id: "handy-computer/cohere-transcribe-03-2026-gguf/cohere-transcribe-03-2026-Q5_K_M.gguf",
        name: "Cohere Transcribe",
        description: "Highest accuracy, heavier download.",
        filename: "cohere-transcribe-03-2026-Q5_K_M.gguf",
        source: hf("handy-computer/cohere-transcribe-03-2026-gguf"),
        size_mb: 1120,
        accuracy_score: 0.92,
        speed_score: 0.63,
        is_recommended: true,
        supported_languages: ["en", "fr", "de", "es", "ja", "zh"],
      }),
      mk({
        id: "handy-computer/whisper-medium-gguf/whisper-medium-Q8_0.gguf",
        name: "Whisper Medium",
        description: "The classic Whisper model, 99 languages.",
        filename: "whisper-medium-Q8_0.gguf",
        source: hf("handy-computer/whisper-medium-gguf"),
        size_mb: 1530,
        accuracy_score: 0.84,
        speed_score: 0.42,
        is_recommended: false,
        supported_languages: ["en", "zh", "de", "es", "ru", "ko", "fr", "ja"],
      }),
      mk({
        id: "handy-computer/Voxtral-Mini-4B-Realtime-2602-gguf/Voxtral-Mini-4B-Realtime-2602-Q5_K_M.gguf",
        name: "Voxtral Mini 4B Realtime",
        description: "Large multilingual realtime model.",
        filename: "Voxtral-Mini-4B-Realtime-2602-Q5_K_M.gguf",
        source: hf("handy-computer/Voxtral-Mini-4B-Realtime-2602-gguf"),
        size_mb: 2970,
        accuracy_score: 0.87,
        speed_score: 0.11,
        is_recommended: false,
        supports_streaming: true,
        supported_languages: ["en", "fr", "es", "de", "ru", "zh", "ja"],
      }),
      mk({
        id: LOCAL_LLM_MODEL_ID,
        name: "Qwen3 0.6B (Post-process)",
        description: "Local post-process engine. Not a voice model.",
        filename: "Qwen3-0.6B-Q8_0.gguf",
        source: hf("Qwen/Qwen3-0.6B-GGUF"),
        size_mb: 644,
        is_downloaded: true,
        engine_type: "LocalLlm",
        accuracy_score: 0,
        speed_score: 0,
        is_recommended: false,
      }),
    ];
  };

  const historyFixture = () => {
    const now = Math.floor(Date.now() / 1000);
    return [
      {
        id: 3,
        file_name: "rec_003.m4a",
        timestamp: now - 400,
        saved: true,
        title: "Standup notes",
        transcription_text:
          "I will take the review of the settings page, and ship the new overlay by Friday.",
        post_processed_text:
          "I'll take the review of the settings page and ship the new overlay by Friday.",
        post_process_prompt: "default_improve_transcriptions",
        post_process_requested: true,
        model_id: PARAKEET_ID,
      },
      {
        id: 2,
        file_name: "rec_002.m4a",
        timestamp: now - 9500,
        saved: false,
        title: "Groceries",
        transcription_text:
          "buy oat milk, coffee beans, and that sourdough from the bakery on fifth street",
        post_processed_text: null,
        post_process_prompt: null,
        post_process_requested: false,
        model_id: PARAKEET_ID,
      },
      {
        id: 1,
        file_name: "rec_001.m4a",
        timestamp: now - 200000,
        saved: false,
        title: "Reminder",
        transcription_text: "call the dentist tomorrow morning before work",
        post_processed_text: null,
        post_process_prompt: null,
        post_process_requested: false,
        model_id: CANARY_ID,
      },
    ];
  };

  const clone = (v) => JSON.parse(JSON.stringify(v));
  const pristineDefaults = defaultSettings();

  const state = {
    appVersion: APP_VERSION,
    permissions: { accessibility: true, microphone: true },
    settings: defaultSettings(),
    models: modelCatalog(),
    history: historyFixture(),
    defaultCommandMatrix: DEFAULT_COMMAND_MATRIX,
  };

  const SCENARIOS = {
    // Returning user with a resident model: the main settings window.
    "settings-default": (s) => {
      s.settings.onboarding_completed = true;
      s.settings.selected_model = PARAKEET_ID;
      s.settings.whats_new_last_seen_version = APP_VERSION;
    },
    // Same user with the two gated tabs enabled.
    "settings-full": (s) => {
      SCENARIOS["settings-default"](s);
      s.settings.debug_mode = true;
      s.settings.post_process_enabled = true;
    },
    // User who upgraded but has not seen the release notes yet.
    "whats-new": (s) => {
      SCENARIOS["settings-default"](s);
      s.settings.whats_new_last_seen_version = "1.0.0";
    },
    // Brand-new install: accessibility and mic both missing.
    "onboarding-fresh": (s) => {
      s.permissions.accessibility = false;
      s.permissions.microphone = false;
    },
    // Accessibility granted, mic still pending.
    "onboarding-mic": (s) => {
      s.permissions.accessibility = true;
      s.permissions.microphone = false;
    },
    // Permissions granted: wizard auto-advances to the model step.
    "onboarding-model": (s) => {
      s.permissions.accessibility = true;
      s.permissions.microphone = true;
    },
    // Overlay webview: returning-user settings, live overlay style.
    overlay: (s) => {
      SCENARIOS["settings-default"](s);
    },
    // Overlay webview with the minimal style.
    "overlay-minimal": (s) => {
      SCENARIOS["settings-default"](s);
      s.settings.overlay_style = "minimal";
    },
  };
  (SCENARIOS[scenarioName] || SCENARIOS["settings-default"])(state);

  // ---- callback + event plumbing (mirrors @tauri-apps/api mocks.js) ----
  const callbacks = new Map();
  let nextCallbackId = 1;
  const listeners = new Map(); // eventId -> { event, callbackId }
  let nextEventId = 1;

  const transformCallback = (callback, once) => {
    const id = nextCallbackId++;
    const fn = once
      ? (data) => {
          callbacks.delete(id);
          callback(data);
        }
      : callback;
    callbacks.set(id, fn);
    return id;
  };

  const emitToListeners = (event, payload) => {
    for (const [eventId, entry] of listeners) {
      if (entry.event !== event) continue;
      const cb = callbacks.get(entry.callbackId);
      if (cb) cb({ event, id: eventId, payload });
    }
  };

  // ---- invoke dispatch ----
  const calls = { count: 0, unknown: [], byCommand: {} };
  const mic = (name) => ({
    index: name === "AirPods Pro" ? "1" : "0",
    name,
    is_default: name === "MacBook Pro Microphone",
  });

  const handlers = {
    // --- app info plugins ---
    "plugin:app|version": () => APP_VERSION,
    "plugin:app|name": () => "VoxBar",
    "plugin:app|tauriVersion": () => "2.9.0",
    "plugin:app|identifier": () => "com.voxbar.app",
    "plugin:os|locale": () => "en-US",
    "plugin:os|version": () => "15.6.1",
    "plugin:os|hostname": () => "macbook.local",
    "plugin:updater|check": () => null,
    "plugin:event|listen": (args) => {
      const eventId = nextEventId++;
      listeners.set(eventId, {
        event: args.event,
        callbackId: args.handler,
      });
      return eventId;
    },
    "plugin:event|unlisten": (args) => {
      for (const [eventId, entry] of listeners) {
        if (entry.event === args.event && eventId === Number(args.eventId)) {
          callbacks.delete(entry.callbackId);
          listeners.delete(eventId);
        }
      }
      return null;
    },
    "plugin:event|emit": (args) => {
      emitToListeners(args.event, args.payload);
      return null;
    },
    "plugin:event|emit_to": (args) => {
      emitToListeners(args.event, args.payload);
      return null;
    },
    "plugin:macos-permissions|check_accessibility_permission": () =>
      state.permissions.accessibility,
    "plugin:macos-permissions|check_microphone_permission": () =>
      state.permissions.microphone,
    "plugin:macos-permissions|request_accessibility_permission": () => {
      // Simulate the user granting in System Settings so the polling
      // onboarding card flips deterministically.
      state.permissions.accessibility = true;
      return null;
    },
    "plugin:macos-permissions|request_microphone_permission": () => {
      state.permissions.microphone = true;
      return null;
    },
    "plugin:opener|open_url": () => null,
    "plugin:opener|open_path": () => null,
    "plugin:opener|reveal_item_in_dir": () => null,
    "plugin:dialog|open": () => null,
    "plugin:fs|read_text_file": () => "",
    "plugin:fs|read_file": () => new ArrayBuffer(0),
    "plugin:autostart|is_enabled": () => state.settings.autostart_enabled,
    "plugin:autostart|enable": () => null,
    "plugin:autostart|disable": () => null,

    // --- settings ---
    get_app_settings: () => clone(state.settings),
    get_default_settings: () => clone(pristineDefaults),
    get_default_command_matrix: () => clone(state.defaultCommandMatrix),

    // --- models ---
    get_available_models: () => clone(state.models),
    get_model_info: (args) => {
      const id = args.modelId ?? args.model_id;
      return clone(state.models.find((m) => m.id === id) ?? null);
    },
    get_current_model: () => state.settings.selected_model || "",
    get_transcription_model_status: () => {
      const m = state.models.find(
        (x) => x.id === state.settings.selected_model,
      );
      return m ? m.name : null;
    },
    get_model_load_status: () => ({
      is_loaded: Boolean(state.settings.selected_model),
      current_model: state.settings.selected_model || null,
    }),
    is_model_loading: () => false,
    set_active_model: (args) => {
      state.settings.selected_model = args.modelId ?? args.model_id ?? "";
      return null;
    },
    set_active_model_deferred: (args) => {
      state.settings.selected_model = args.modelId ?? args.model_id ?? "";
      return null;
    },
    rescan_local_models: () => clone(state.models),
    download_model: () => null,
    cancel_download: () => null,
    delete_model: () => null,
    unload_model_manually: () => null,
    get_local_llm_model_status: () => ({
      downloaded: true,
      downloading: false,
      size_mb: 644,
      progress: 100,
    }),
    download_local_llm_model: () => null,
    delete_local_llm_model: () => null,
    get_available_accelerators: () => ({
      transcribe: ["auto", "cpu", "metal"],
      ort: ["auto", "cpu"],
      gpu_devices: [
        { id: "metal", name: "Apple Silicon GPU", total_vram_mb: 16384 },
      ],
    }),
    check_apple_intelligence_available: () => true,
    resolve_hf_model: (args) => ({
      repo_id: "mistral/example-asr",
      revision: "9f2a1c",
      files: [
        { filename: "model-Q8_0.gguf", size_bytes: 66060288 },
        { filename: "model-Q5_K_M.gguf", size_bytes: 44359680 },
      ],
      suggested_filename: "model-Q8_0.gguf",
    }),
    add_hf_model: () => null,

    // --- history ---
    get_history_entries: (args) => {
      const limit = args.limit ?? state.history.length;
      return { entries: clone(state.history.slice(0, limit)), has_more: false };
    },
    get_audio_file_path: (args) =>
      `/mock-recordings/${args.fileName ?? args.file_name ?? ""}`,
    toggle_history_entry_saved: (args) => {
      const entry = state.history.find((e) => e.id === args.id);
      if (entry) entry.saved = !entry.saved;
      return null;
    },
    delete_history_entry: () => null,
    retry_history_entry_transcription: () => null,
    open_recordings_folder: () => null,

    // --- audio devices ---
    get_available_microphones: () => [
      mic("MacBook Pro Microphone"),
      mic("AirPods Pro"),
    ],
    get_available_output_devices: () => [
      { index: "0", name: "MacBook Pro Speakers", is_default: true },
      { index: "1", name: "AirPods Pro", is_default: false },
    ],
    get_microphone_channels: (args) =>
      (args.deviceName ?? args.device_name) === "AirPods Pro" ? 2 : 1,
    get_microphone_mode: () => state.settings.always_on_microphone,
    set_selected_microphone: (args) => {
      state.settings.selected_microphone = args.deviceName ?? null;
      return null;
    },
    set_selected_channel: (args) => {
      state.settings.selected_channel = args.channel ?? null;
      return null;
    },
    set_clamshell_microphone: (args) => {
      state.settings.clamshell_microphone = args.deviceName ?? null;
      return null;
    },
    set_selected_output_device: (args) => {
      state.settings.selected_output_device = args.deviceName ?? null;
      return null;
    },
    update_microphone_mode: (args) => {
      state.settings.always_on_microphone = Boolean(args.alwaysOn);
      return null;
    },
    set_post_process_provider: (args) => {
      state.settings.post_process_provider_id =
        args.providerId ?? args.provider_id ?? "local";
      return null;
    },
    set_post_process_selected_prompt: (args) => {
      state.settings.post_process_selected_prompt_id = args.id ?? null;
      return null;
    },
    change_post_process_base_url_setting: (args) => {
      const provider = state.settings.post_process_providers.find(
        (p) => p.id === (args.providerId ?? args.provider_id),
      );
      if (provider) provider.base_url = args.baseUrl ?? provider.base_url;
      return null;
    },
    change_post_process_api_key_setting: (args) => {
      state.settings.post_process_api_keys[
        args.providerId ?? args.provider_id
      ] = args.apiKey ?? "";
      return null;
    },
    change_post_process_model_setting: (args) => {
      state.settings.post_process_models[args.providerId ?? args.provider_id] =
        args.model ?? "";
      return null;
    },

    // --- misc backend state ---
    is_recording: () => false,
    is_laptop: () => true,
    is_portable: () => false,
    is_app_translocated: () => false,
    is_update_checks_locked: () => false,
    get_windows_microphone_permission_status: () => ({
      supported: true,
      overall_access: "allowed",
      device_access: "allowed",
      app_access: "allowed",
      desktop_app_access: "allowed",
    }),
    get_secure_input_status: () => ({
      enabled: false,
      sustained: false,
      culprit_pid: null,
      culprit_name: null,
      fallback_active: false,
      covered_bindings: [],
      degraded_bindings: [],
      uncovered_bindings: [],
      recorder_blocked: false,
    }),
    run_keyboard_diagnostic: () => ({
      secure_input_enabled: false,
      culprit_pid: null,
      culprit_name: null,
      key_down: 42,
      key_up: 42,
      flags_changed: 7,
      mouse: 0,
      duration_ms: 5000,
    }),
    get_keyboard_implementation: () =>
      state.settings.keyboard_implementation ?? "handy_keys",
    get_available_typing_tools: () => [],
    get_log_dir_path: () => "/Users/dev/Library/Logs/com.voxbar.app",
    get_app_dir_path: () =>
      "/Users/dev/Library/Application Support/com.voxbar.app",
    open_log_dir: () => null,
    open_app_data_dir: () => null,
    initialize_enigo: () => null,
    initialize_shortcuts: () => null,
    show_main_window_command: () => null,
    open_microphone_privacy_settings: () => null,
    play_test_sound: () => null,
    check_custom_sounds: () => ({ start: false, stop: false }),
    cancel_operation: () => null,
    suspend_all_bindings: () => null,
    resume_all_bindings: () => null,
    start_handy_keys_recording: () => null,
    stop_handy_keys_recording: () => null,
    log_update_decision: () => null,
    set_log_level: (args) => {
      state.settings.log_level = args.level ?? "debug";
      return null;
    },
    change_binding: (args) => {
      const id = args.id;
      const binding = state.settings.bindings[id];
      if (binding) binding.current_binding = args.binding ?? "";
      return {
        success: true,
        binding: binding ? clone(binding) : null,
        error: null,
      };
    },
    reset_binding: (args) => {
      const binding = state.settings.bindings[args.id];
      if (binding) binding.current_binding = binding.default_binding;
      return {
        success: true,
        binding: binding ? clone(binding) : null,
        error: null,
      };
    },
    change_keyboard_implementation_setting: (args) => {
      state.settings.keyboard_implementation =
        args.implementation ?? "handy_keys";
      return { success: true, reset_bindings: [] };
    },
    set_model_unload_timeout: (args) => {
      state.settings.model_unload_timeout = args.timeout ?? "min_5";
      return null;
    },
    set_model_unload_timeout_custom_seconds: (args) => {
      state.settings.model_unload_timeout = {
        custom: { seconds: args.seconds ?? 45 },
      };
      return null;
    },
    update_command_matrix: (args) => {
      state.settings.command_phrases = args.entries ?? null;
      return null;
    },
    fetch_post_process_models: (args) => {
      const providerId = args.providerId ?? args.provider_id;
      const table = {
        openai: ["gpt-4.1-mini", "gpt-4o-mini", "gpt-4.1"],
        zai: ["glm-4.7-flash", "glm-4.7"],
        openrouter: ["openai/gpt-4o-mini", "anthropic/claude-3.5-haiku"],
      };
      return table[providerId] ?? [];
    },
  };

  // Settings mutation commands: change_<x>_setting / set_post_process_* /
  // update_<x> map onto state.settings generically.
  const SETTING_KEY_BY_COMMAND = {
    change_audio_feedback_setting: "audio_feedback",
    change_audio_feedback_volume_setting: "audio_feedback_volume",
    change_sound_theme_setting: "sound_theme",
    change_start_hidden_setting: "start_hidden",
    change_autostart_setting: "autostart_enabled",
    change_update_checks_setting: "update_checks_enabled",
    change_update_policy_setting: "update_policy",
    change_show_whats_new_on_update_setting: "show_whats_new_on_update",
    change_whats_new_last_seen_version_setting: "whats_new_last_seen_version",
    change_shortcut_activation_setting: "shortcut_activation",
    change_hold_threshold_ms_setting: "hold_threshold_ms",
    change_translate_to_english_setting: "translate_to_english",
    change_selected_language_setting: "selected_language",
    change_overlay_position_setting: "overlay_position",
    change_overlay_style_setting: "overlay_style",
    change_debug_mode_setting: "debug_mode",
    change_theme_setting: "theme",
    change_accent_color_setting: "accent_color",
    change_app_language_setting: "app_language",
    change_experimental_enabled_setting: "experimental_enabled",
    change_lazy_stream_close_setting: "lazy_stream_close",
    change_memory_pressure_guard_setting: "memory_pressure_guard",
    change_auto_fallback_setting: "auto_fallback",
    change_menu_bar_model_title_setting: "menu_bar_model_title",
    change_show_history_model_setting: "show_history_model",
    change_show_tray_icon_setting: "show_tray_icon",
    change_paste_method_setting: "paste_method",
    change_typing_tool_setting: "typing_tool",
    change_external_script_path_setting: "external_script_path",
    change_clipboard_handling_setting: "clipboard_handling",
    change_auto_submit_setting: "auto_submit",
    change_auto_submit_key_setting: "auto_submit_key",
    change_post_process_enabled_setting: "post_process_enabled",
    change_mute_while_recording_setting: "mute_while_recording",
    change_append_trailing_space_setting: "append_trailing_space",
    change_word_correction_threshold_setting: "word_correction_threshold",
    change_paste_delay_ms_setting: "paste_delay_ms",
    change_paste_delay_after_ms_setting: "paste_delay_after_ms",
    change_reliable_paste_setting: "reliable_paste",
    change_vad_enabled_setting: "vad_enabled",
    change_vad_backend_setting: "vad_backend",
    change_filler_word_removal_enabled_setting: "filler_word_removal_enabled",
    change_spoken_punctuation_setting: "spoken_punctuation",
    change_auto_interpret_commands_setting: "auto_interpret_commands",
    change_terminal_punctuation_setting: "terminal_punctuation",
    change_voice_deletion_commands_setting: "voice_deletion_commands",
    change_preview_before_paste_setting: "preview_before_paste",
    change_delete_last_word_enabled_setting: "delete_last_word_enabled",
    change_undo_enabled_setting: "undo_enabled",
    change_command_mode_enabled_setting: "command_mode_enabled",
    change_chinese_script_setting: "chinese_script",
    change_number_format_setting: "number_format",
    change_transcribe_accelerator_setting: "transcribe_accelerator",
    change_ort_accelerator_setting: "ort_accelerator",
    change_transcribe_gpu_device: "transcribe_gpu_device",
    change_extra_recording_buffer_setting: "extra_recording_buffer_ms",
    change_streaming_release_tail_setting: "streaming_release_tail_ms",
    set_post_process_timeout: "post_process_timeout_secs",
    set_post_process_provider: "post_process_provider_id",
    set_post_process_selected_prompt: "post_process_selected_prompt_id",
    update_custom_words: "custom_words",
    update_history_limit: "history_limit",
    update_recording_retention_period: "recording_retention_period",
  };

  // Arg keys used by the generated bindings, in priority order (verified
  // against the signatures in src/bindings.ts).
  const ARG_KEYS = [
    "enabled",
    "alwaysOn",
    "value",
    "theme",
    "accent",
    "language",
    "level",
    "timeout",
    "seconds",
    "policy",
    "activation",
    "ms",
    "volume",
    "method",
    "tool",
    "path",
    "handling",
    "key",
    "period",
    "words",
    "entries",
    "limit",
    "threshold",
    "implementation",
    "script",
    "backend",
    "format",
    "style",
    "position",
    "accelerator",
    "device",
  ];
  const firstArgValue = (args) => {
    for (const k of ARG_KEYS) {
      if (args[k] !== undefined) return args[k];
    }
    return null;
  };

  // Real Tauri IPC always resolves on a macrotask (webview -> Rust ->
  // webview), never a microtask. Resolving synchronously-amplifies the
  // app's bounded refresh cycles (e.g. AccessibilityOnboarding's
  // completeOnboarding re-entry before its 300ms timer lands) into a
  // synchronous render loop that kills the renderer. A small macrotask
  // delay reproduces real IPC pacing and keeps those cycles bounded.
  const ipcDelay = (value) =>
    new Promise((resolve) => {
      setTimeout(() => resolve(value), 3);
    });

  window.__TAURI_INTERNALS__ = {
    invoke: (cmd, args = {}) => {
      calls.count += 1;
      calls.byCommand[cmd] = (calls.byCommand[cmd] ?? 0) + 1;
      const handler = handlers[cmd];
      try {
        if (handler) return ipcDelay(handler(args));
        const settingKey = SETTING_KEY_BY_COMMAND[cmd];
        if (settingKey) {
          state.settings[settingKey] = firstArgValue(args);
          return ipcDelay(null);
        }
        if (cmd.startsWith("plugin:channels|") || cmd.startsWith("plugin:")) {
          // Silent no-op for untouched plugin surfaces.
          return ipcDelay(null);
        }
        calls.unknown.push(cmd);
        return ipcDelay(null);
      } catch (error) {
        return Promise.reject(String(error));
      }
    },
    transformCallback,
    unregisterCallback: (id) => {
      callbacks.delete(id);
    },
    convertFileSrc: (filePath) =>
      `asset://mock/${encodeURIComponent(filePath)}`,
    metadata: {
      currentWindow: {
        label: window.location.pathname.includes("overlay")
          ? "recording-overlay"
          : "main",
      },
      currentWebview: {
        label: window.location.pathname.includes("overlay")
          ? "recording-overlay"
          : "main",
      },
    },
  };

  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener: (event, eventId) => {
      for (const [id, entry] of listeners) {
        if (entry.event === event && id === Number(eventId)) {
          callbacks.delete(entry.callbackId);
          listeners.delete(id);
        }
      }
    },
  };

  // plugin-os synchronous injection (values mirror a real macOS arm64 host).
  window.__TAURI_OS_PLUGIN_INTERNALS__ = {
    platform: "macos",
    family: "unix",
    os_type: "macos",
    arch: "aarch64",
    version: "15.6.1",
    exe_extension: "",
  };

  // Legacy global referenced by some tooling; harmless for the app itself.
  window.__TAURI__ = window.__TAURI__ ?? {};

  // Overlay backdrop: the real overlay webview is transparent. A solid
  // neutral backdrop makes the floating card judgeable in a PNG.
  if (window.location.pathname.includes("overlay")) {
    const style = document.createElement("style");
    style.textContent =
      "html,body{background:#26262b !important;} #root{background:transparent !important;}";
    document.addEventListener("DOMContentLoaded", () => {
      document.head.appendChild(style);
      document.documentElement.style.background = "#26262b";
      document.body.style.background = "#26262b";
    });
  }

  // Deterministic mic levels so the waveform renders bars, not a flat line.
  let micTimer = null;
  const startMicLevels = () => {
    if (micTimer) return;
    let t = 0;
    micTimer = setInterval(() => {
      t += 1;
      const levels = Array.from({ length: 16 }, (_, i) => {
        const wave =
          0.45 +
          0.35 * Math.sin((t + i * 1.7) / 3.1) * Math.cos((t - i * 0.6) / 4.3);
        return Math.max(0.08, Math.min(0.95, Math.abs(wave)));
      });
      emitToListeners("mic-level", levels);
    }, 120);
  };
  const stopMicLevels = () => {
    if (micTimer) clearInterval(micTimer);
    micTimer = null;
  };

  // Runtime control surface for the driver.
  window.__voxshot = {
    scenario: scenarioName,
    state,
    calls,
    emit: emitToListeners,
    startMicLevels,
    stopMicLevels,
    grantPermission: (name) => {
      state.permissions[name] = true;
    },
  };
})();
