import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { CircleAlert, Loader2, Plus, X } from "lucide-react";
import {
  commands,
  type HfModelError,
  type HfModelResolution,
} from "@/bindings";
import { formatModelSize } from "@/lib/utils/format";

type AddPhase = "idle" | "resolving" | "picking" | "adding" | "added";

// Bytes (from the HF listing) to the MB formatModelSize expects.
const bytesToMb = (bytes: number | null): number | null =>
  bytes === null ? null : Math.max(1, Math.round(bytes / (1024 * 1024)));

interface AddLlmModelFromHuggingFaceProps {
  /// Called after a successful add (or a cleanup) so the section re-reads
  /// the authoritative list.
  onChanged: () => void;
}

// The post-process sibling of the voice tab's AddModelFromHuggingFace:
// same paste-a-repo flow and error wording, pointed at the LLM resolve/add
// commands so the architecture gate runs against the post-process
// allowlist (a whisper GGUF is refused here exactly like a qwen3 GGUF is
// refused on the voice path).
export const AddLlmModelFromHuggingFace: React.FC<
  AddLlmModelFromHuggingFaceProps
> = ({ onChanged }) => {
  const { t } = useTranslation();

  const [input, setInput] = useState("");
  const [phase, setPhase] = useState<AddPhase>("idle");
  const [error, setError] = useState<string | null>(null);
  const [resolution, setResolution] = useState<HfModelResolution | null>(null);

  const busy = phase === "resolving" || phase === "adding";

  const describeError = (err: HfModelError): string => {
    if (typeof err === "string") {
      return t("settings.models.addFromHf.errors.cancelled");
    }
    if ("InvalidInput" in err) {
      return t("settings.models.addFromHf.errors.invalidInput", {
        detail: err.InvalidInput.detail,
      });
    }
    if ("RepoNotFound" in err) {
      return t("settings.models.addFromHf.errors.notFound", {
        repo: err.RepoNotFound.repo_id,
      });
    }
    if ("Inaccessible" in err) {
      return t("settings.models.addFromHf.errors.inaccessible", {
        repo: err.Inaccessible.repo_id,
      });
    }
    if ("FileNotFound" in err) {
      return t("settings.models.addFromHf.errors.fileNotFound", {
        repo: err.FileNotFound.repo_id,
        filename: err.FileNotFound.filename,
      });
    }
    if ("NoGgufFiles" in err) {
      return t("settings.models.addFromHf.errors.noGguf", {
        repo: err.NoGgufFiles.repo_id,
      });
    }
    if ("Network" in err) {
      return t("settings.models.addFromHf.errors.network", {
        detail: err.Network.detail,
      });
    }
    if ("DownloadFailed" in err) {
      return t("settings.models.addFromHf.errors.downloadFailed", {
        detail: err.DownloadFailed.detail,
      });
    }
    if ("UnsupportedArchitecture" in err) {
      const { architecture, supported } = err.UnsupportedArchitecture;
      return architecture
        ? t("settings.models.addFromHf.errors.unsupportedArch", {
            arch: architecture,
            supported: supported.join(", "),
          })
        : t("settings.models.addFromHf.errors.unsupportedArchUnknown");
    }
    return t("settings.models.addFromHf.errors.generic");
  };

  const startAdd = async (
    repoId: string,
    filename: string,
    revision: string | null,
  ) => {
    setPhase("adding");
    setError(null);
    const result = await commands.addLlmHfModel(repoId, filename, revision);
    if (result.status === "ok") {
      setPhase("added");
      setInput("");
      setResolution(null);
      await onChanged();
    } else {
      setError(describeError(result.error));
      setPhase("idle");
      // Drop any provisional card state the backend already cleaned up.
      await onChanged();
    }
  };

  const handleResolve = async () => {
    const trimmed = input.trim();
    if (!trimmed || busy) {
      return;
    }
    setPhase("resolving");
    setError(null);
    setResolution(null);
    const result = await commands.resolveLlmHfModel(trimmed);
    if (result.status === "error") {
      setError(describeError(result.error));
      setPhase("idle");
      return;
    }
    const res = result.data;
    // One file: no picker, download it right away.
    if (res.files.length === 1) {
      await startAdd(res.repo_id, res.files[0].filename, res.revision);
      return;
    }
    setResolution(res);
    setPhase("picking");
  };

  const handleKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "Enter") {
      event.preventDefault();
      handleResolve();
    }
  };

  const reset = () => {
    setPhase("idle");
    setResolution(null);
    setError(null);
  };

  return (
    <div className="rounded-lg border border-mid-gray/40 bg-mid-gray/5 p-3 space-y-2">
      <div>
        <h3 className="text-sm font-medium">
          {t("settings.postProcessing.models.addFromHf.title")}
        </h3>
        <p className="text-xs text-text/50 mt-0.5">
          {t("settings.postProcessing.models.addFromHf.description")}
        </p>
      </div>

      <div className="flex items-center gap-2">
        <input
          type="text"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={t("settings.models.addFromHf.placeholder")}
          disabled={busy}
          className="flex-1 min-w-0 px-3 py-2 text-sm bg-mid-gray/10 border border-mid-gray/40 rounded-lg focus:outline-none focus:ring-1 focus:ring-logo-primary placeholder:text-text/40 disabled:opacity-50"
        />
        <button
          type="button"
          onClick={handleResolve}
          disabled={!input.trim() || busy}
          className="flex items-center gap-1.5 px-3 py-2 text-sm font-medium rounded-lg border border-transparent bg-background-ui text-white hover:bg-background-ui/70 transition-colors disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer whitespace-nowrap"
        >
          {phase === "resolving" || phase === "adding" ? (
            <Loader2 className="w-3.5 h-3.5 animate-spin" />
          ) : (
            <Plus className="w-3.5 h-3.5" />
          )}
          {t("settings.models.addFromHf.add")}
        </button>
      </div>

      {phase === "picking" && resolution && (
        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <span className="text-xs text-text/60">
              {t("settings.models.addFromHf.pickFile", {
                repo: resolution.repo_id,
              })}
            </span>
            <button
              type="button"
              onClick={reset}
              aria-label={t("settings.models.addFromHf.cancel")}
              className="text-text/50 hover:text-text transition-colors cursor-pointer"
            >
              <X className="w-3.5 h-3.5" />
            </button>
          </div>
          <div className="max-h-52 overflow-y-auto rounded-lg border border-mid-gray/30 divide-y divide-mid-gray/20">
            {resolution.files.map((file) => {
              const suggested = file.filename === resolution.suggested_filename;
              return (
                <button
                  key={file.filename}
                  type="button"
                  onClick={() =>
                    startAdd(
                      resolution.repo_id,
                      file.filename,
                      resolution.revision,
                    )
                  }
                  className="w-full flex items-center justify-between gap-3 px-3 py-2 text-sm text-left hover:bg-mid-gray/10 transition-colors cursor-pointer"
                >
                  <span className="flex items-center gap-2 min-w-0">
                    <span className="truncate">{file.filename}</span>
                    {suggested && (
                      <span className="shrink-0 px-1.5 py-0.5 text-xs font-medium rounded bg-logo-primary/20 text-logo-primary">
                        {t("settings.models.addFromHf.recommended")}
                      </span>
                    )}
                  </span>
                  <span className="shrink-0 text-xs text-text/50">
                    {formatModelSize(bytesToMb(file.size_bytes))}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
      )}

      {phase === "resolving" && (
        <p className="flex items-center gap-1.5 text-xs text-text/60">
          <Loader2 className="w-3 h-3 animate-spin" />
          {t("settings.models.addFromHf.status.resolving")}
        </p>
      )}
      {phase === "adding" && (
        <p className="flex items-center gap-1.5 text-xs text-text/60">
          <Loader2 className="w-3 h-3 animate-spin" />
          {t("settings.models.addFromHf.status.adding")}
        </p>
      )}
      {phase === "added" && (
        <p className="text-xs text-text/60">
          {t("settings.postProcessing.models.addFromHf.added")}
        </p>
      )}
      {error && (
        <p className="flex items-start gap-1.5 text-xs text-red-400">
          <CircleAlert className="w-3.5 h-3.5 shrink-0 mt-0.5" />
          <span className="break-all">{error}</span>
        </p>
      )}
    </div>
  );
};
