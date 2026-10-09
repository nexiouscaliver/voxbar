// Payload of the shared "model-download-failed" event emitted by the Rust
// download wrappers (commands/models.rs for voice models,
// commands/local_llm.rs for the pinned post-process model). `name` is
// optional: only the local post-process model emits it today, because it
// is filtered out of the store's model list and the shared toast would
// otherwise fall back to its raw registry id.
export interface ModelDownloadFailedEvent {
  model_id: string;
  error: string;
  name?: string;
}

// The model name the shared download-failure toast shows: prefer the
// event's display name, then the registry name, then the raw id (the
// pre-event-name behavior, kept as the last resort).
export function downloadFailedModelName(
  payload: ModelDownloadFailedEvent,
  models: ReadonlyArray<{ id: string; name: string }>,
): string {
  return (
    payload.name ??
    models.find((m) => m.id === payload.model_id)?.name ??
    payload.model_id
  );
}
