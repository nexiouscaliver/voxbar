/**
 * The Prompt Studio's cross-screen navigation events (v1.6.0 W1-L6).
 *
 * The per-template Test button (Post Processing settings) and any future
 * surface open the studio through `openPromptStudio`: App.tsx hears the
 * open event, switches the sidebar section, and re-dispatches the preload
 * event after the studio has mounted (the "open-settings-unload-timeout"
 * precedent), so the studio lands with that template already checked.
 *
 * Namespaced on window events (not Tauri emits) because both ends live in
 * the main webview; the constants live here so the three call sites cannot
 * drift apart on the string.
 */
export const OPEN_PROMPT_STUDIO_EVENT = "voxbar:open-prompt-studio";
export const PRELOAD_PROMPT_STUDIO_EVENT = "voxbar:preload-prompt-studio";

export interface PromptStudioEventDetail {
  promptId: string;
}

/** Open the Prompt Studio with one template preselected. */
export const openPromptStudio = (promptId: string): void => {
  window.dispatchEvent(
    new CustomEvent<PromptStudioEventDetail>(OPEN_PROMPT_STUDIO_EVENT, {
      detail: { promptId },
    }),
  );
};
