import React, { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { Search } from "lucide-react";
import { commands, type LlmModelEntry } from "@/bindings";
import { SettingsGroup } from "@/components/ui";
import { LlmModelCard, LocalLlmModelRow } from "./LocalLlmModelRow";
import { AddLlmModelFromHuggingFace } from "./AddLlmModelFromHuggingFace";
import {
  LOCAL_LLM_MODEL_ID,
  splitLlmModels,
  showPinnedOnlyRow,
} from "./localLlmRouting";

// Payload of the shared "model-download-progress" event
// (DownloadProgress in src-tauri/src/managers/model.rs).
interface DownloadProgressEvent {
  model_id: string;
  percentage: number;
}

// The Post-process Models section (voice Models-tab parity for the local
// engine): search, downloaded cards with the Active badge and Use/Delete,
// downloadable cards, and the add-from-HuggingFace flow pointed at the LLM
// commands. Status refreshes on mount, on command completion, and on the
// shared model-manager events - the same stability-over-looks contract as
// the pinned row (no polling loop).
export const PostProcessModelsSection: React.FC = () => {
  const { t } = useTranslation();
  const [entries, setEntries] = useState<LlmModelEntry[] | null>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [progress, setProgress] = useState<Record<string, number>>({});

  const refresh = useCallback(async () => {
    const result = await commands.getAvailableLlmModels();
    if (result.status === "ok") {
      setEntries(result.data);
    }
    // On error the section keeps its last known state, like the row does.
  }, []);

  useEffect(() => {
    void refresh();

    const unlistenProgress = listen<DownloadProgressEvent>(
      "model-download-progress",
      (event) => {
        setProgress((prev) => ({
          ...prev,
          [event.payload.model_id]: event.payload.percentage,
        }));
      },
    );

    // Registry-wide refresh signals: a download finished or was cancelled,
    // the list changed elsewhere, or the selection moved (settings UI,
    // tray). Re-read the authoritative snapshot.
    const unlisteners = [
      "model-download-complete",
      "model-download-cancelled",
      "models-updated",
      "post-process-model-changed",
    ].map((name) =>
      listen(name, () => {
        void refresh();
      }),
    );

    return () => {
      unlistenProgress.then((fn) => fn());
      for (const p of unlisteners) {
        p.then((fn) => fn());
      }
    };
  }, [refresh]);

  const selectedId = useMemo(() => {
    const selected = entries?.find((entry) => entry.selected);
    return selected?.info.id ?? LOCAL_LLM_MODEL_ID;
  }, [entries]);

  // The off path: until the user downloads or selects anything beyond the
  // pinned model, the section renders the original pinned row unchanged.
  if (entries !== null && showPinnedOnlyRow(entries, selectedId)) {
    return <LocalLlmModelRow />;
  }

  const q = searchQuery.trim().toLowerCase();
  const filtered = (entries ?? []).filter((entry) => {
    if (!q) return true;
    const haystack =
      `${entry.info.name} ${entry.info.description} ${entry.publisher} ${entry.quant}`.toLowerCase();
    return haystack.includes(q);
  });
  const { downloaded, available } = splitLlmModels(filtered);

  return (
    <SettingsGroup title={t("settings.postProcessing.models.title")}>
      <div className="space-y-3">
        <p className="text-xs text-text/50">
          {t("settings.postProcessing.models.description")}
        </p>

        <div className="relative">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-text/40 pointer-events-none" />
          <input
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder={t("settings.postProcessing.models.searchPlaceholder")}
            className="w-full pl-9 pr-3 py-2 text-sm bg-mid-gray/10 border border-mid-gray/40 rounded-lg focus:outline-none focus:ring-1 focus:ring-logo-primary placeholder:text-text/40"
          />
        </div>

        <AddLlmModelFromHuggingFace onChanged={refresh} />

        {entries === null ? (
          <p className="text-xs text-text/50">
            {t("settings.postProcessing.local.modelStatus.checking")}
          </p>
        ) : (
          <>
            <div className="space-y-3">
              <h3 className="text-sm font-medium text-text/60">
                {t("settings.postProcessing.models.downloaded")}
              </h3>
              {downloaded.map((entry) => (
                <LlmModelCard
                  key={entry.info.id}
                  entry={entry}
                  progress={progress[entry.info.id]}
                  onChanged={refresh}
                />
              ))}
            </div>

            {available.length > 0 && (
              <div className="space-y-3">
                <h3 className="text-sm font-medium text-text/60">
                  {t("settings.postProcessing.models.available")}
                </h3>
                {available.map((entry) => (
                  <LlmModelCard
                    key={entry.info.id}
                    entry={entry}
                    progress={progress[entry.info.id]}
                    onChanged={refresh}
                  />
                ))}
              </div>
            )}

            {filtered.length === 0 && (
              <div className="text-center py-6 text-text/50 text-sm">
                {t("settings.postProcessing.models.noMatch")}
              </div>
            )}
          </>
        )}
      </div>
    </SettingsGroup>
  );
};
