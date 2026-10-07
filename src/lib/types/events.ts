export interface ModelStateEvent {
  event_type: string;
  model_id?: string;
  model_name?: string;
  error?: string;
}

/** One-shot "model-fallback" payload: the selected model was refused by the
 * memory-pressure guard and the fallback model is being loaded instead. */
export interface ModelFallbackEvent {
  requested_model_name: string;
  fallback_model_name: string;
}

export interface RecordingErrorEvent {
  error_type: string;
  detail?: string;
}
