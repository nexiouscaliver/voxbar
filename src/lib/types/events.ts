export interface MemoryGateRefusal {
  forecast_bytes: number;
  free_bytes: number;
  headroom_bytes: number;
}

export interface ModelStateEvent {
  event_type: string;
  model_id?: string;
  model_name?: string;
  error?: string;
  /** Structured memory-gate refusal numbers; present only on loading_failed
   * events emitted by the gate's Refuse path. */
  memory_gate?: MemoryGateRefusal;
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
