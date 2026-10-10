import { listen } from "@tauri-apps/api/event";
import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import "./RecordingOverlay.css";
import { commands, events } from "@/bindings";
import type {
  OverlayNoticeEvent,
  StreamPhase,
  StreamPhaseEvent,
  StreamTextEvent,
  StreamWorkKind,
} from "@/bindings";
import i18n, { syncLanguageFromSettings } from "@/i18n";
import { getLanguageDirection } from "@/lib/utils/rtl";
import {
  normalizePlacement,
  overlayCacheStorage,
  readCachedPlacement,
  writeCachedLanguage,
  writeCachedPlacement,
} from "./overlayBootstrap";

type OverlayState =
  | "recording"
  | "streaming"
  | "transcribing"
  | "processing"
  | "preview";

// How long a notice stays on the card before it dismisses itself. Errors get
// the full window; the delayed overlay hide on error paths gives them the
// same order of time to be read.
const NOTICE_DISMISS_MS = 5000;

// Number of reactive bars in the waveform (the simple, smoothed style shared by
// every overlay form). Mic levels arrive as 16 FFT buckets; we take the first N.
const WAVE_BARS = 9;

// Localize one notice. Codes deliberately reuse the strings the main-window
// toasts already ship (one voice for the same failure on both surfaces); the
// keys below exist in every locale. The post-process skips reuse the toast
// copy for the same reason.
function noticeMessage(
  notice: OverlayNoticeEvent,
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  switch (notice.code) {
    case "no_model_selected":
      return t("errors.noModelSelected");
    case "microphone_permission_denied":
      return t("errors.micPermissionDenied.generic");
    case "no_input_device":
      return t("errors.noInputDevice");
    case "recording_failed":
      return t("errors.recordingFailed", { error: notice.detail ?? "" });
    case "transcription_failed":
      return t("overlay.notice.transcriptionFailed");
    case "paste_failed":
      return t("errors.pasteFailed");
    case "model_load_failed":
      return t("errors.modelLoadFailed", {
        model: notice.detail ?? t("errors.modelLoadFailedUnknown"),
      });
    case "model_fallback":
      return t("errors.modelFallback", { model: notice.detail ?? "" });
    case "post_process_memory_gate":
      return t("toast.postProcessSkip.memoryGate", {
        detail: notice.detail ?? "",
      });
    case "post_process_download_missing":
      return t("toast.postProcessSkip.downloadMissing");
    case "post_process_engine_failed":
      return t("toast.postProcessSkip.engineFailed");
    case "post_process_timeout":
      return t("toast.postProcessSkip.timeout");
    case "post_process_length_guard":
      return t("toast.postProcessSkip.lengthGuard");
    case "post_process_too_long":
      return t("toast.postProcessSkip.tooLong");
    case "delete_last_word_no_session":
      return t("overlay.notice.deleteLastWordNoSession");
    case "delete_last_word_no_buffer":
      return t("overlay.notice.deleteLastWordNoBuffer");
    case "undo_no_session":
      return t("overlay.notice.undoNoSession");
    case "undo_no_buffer":
      return t("overlay.notice.undoNoBuffer");
    case "binding_busy":
      return t("overlay.notice.bindingBusy");
    case "wayland_tauri_hotkeys":
      return t("overlay.notice.waylandTauriHotkeys");
    case "gnome_overlay_fallback":
      return t("overlay.notice.gnomeOverlayFallback");
    default:
      return t("overlay.notice.generic");
  }
}

// Which tone a notice renders in. Red is reserved for failures; a
// model_fallback is a successful degradation (dictation continued on the
// smaller model), so even when the backend tags it "error" it wears the
// neutral info chip.
function noticeTone(notice: OverlayNoticeEvent): "err" | "info" {
  if (notice.code === "model_fallback") return "info";
  return notice.kind === "error" ? "err" : "info";
}

const RecordingOverlay: React.FC = () => {
  const { t } = useTranslation();
  const [isVisible, setIsVisible] = useState(false);
  const [state, setState] = useState<OverlayState>("recording");
  // `Stream::play()` returning does not mean hardware callbacks are flowing.
  // Stay visually in an arming state until the backend processes the first
  // actual microphone sample chunk.
  const [captureReady, setCaptureReady] = useState(false);
  const [levels, setLevels] = useState<number[]>(Array(WAVE_BARS).fill(0));
  const [streamText, setStreamText] = useState<StreamTextEvent>({
    committed: "",
    tentative: "",
  });
  const [phase, setPhase] = useState<StreamPhase>("listening");
  const [workKind, setWorkKind] = useState<StreamWorkKind>("transcribing");
  const [elapsed, setElapsed] = useState(0);
  // Bumped on each new streaming session so the Live card remounts fresh (replays
  // the pop-in, and never animates in from the previous panel's open size).
  const [session, setSession] = useState(0);
  // Overlay placement (top vs bottom of the screen). The Live panel grows downward
  // from a top overlay (oldest line under the pill) and upward from a bottom one.
  // Seeded from the shared cache so the first paint after show-overlay already
  // has the right orientation; reconciled with AppSettings in the background.
  const [position, setPosition] = useState<"top" | "bottom">(() =>
    readCachedPlacement(overlayCacheStorage()),
  );

  // Refresh language + placement from the backend without blocking paint:
  // syncLanguageFromSettings applies the persisted language (never throws),
  // then one settings read updates the placement cache and state. Every
  // write also refreshes the cache the next boot paints from. Stable (only
  // module imports + the setState family), so the mount effect can hold it.
  const reconcileFromSettings = useCallback(async () => {
    await syncLanguageFromSettings();
    writeCachedLanguage(overlayCacheStorage(), i18n.language);
    try {
      const result = await commands.getAppSettings();
      if (result.status === "ok") {
        const placement = normalizePlacement(result.data.overlay_position);
        writeCachedPlacement(overlayCacheStorage(), placement);
        setPosition(placement);
      }
    } catch {
      // Keep the cached placement if settings can't be read.
    }
  }, []);
  // True once live text overflows the cap. A top overlay fades its top edge only
  // while overflowing, so the resting first line stays crisp flush under the pill.
  const [overflowing, setOverflowing] = useState(false);
  // Text a buffer-side deletion just removed (delete-word hotkey or a
  // command-mode DeleteWord / DeleteLine), shown on a transient chip for
  // about 1.5 s so a deletion is never invisible. Null when idle.
  const [removedText, setRemovedText] = useState<string | null>(null);
  const removedTimerRef = useRef<number | null>(null);
  // The latest backend notice (failure or fallback), shown as a row on the
  // card. Per the channel's display rule this renders ONLY while the card is
  // already visible: the backend never force-shows the overlay for a notice,
  // and a notice that arrives while hidden is carried by the error sound and
  // the main-window toast instead. Cleared on hide and on fresh sessions.
  const [notice, setNotice] = useState<OverlayNoticeEvent | null>(null);
  const noticeTimerRef = useRef<number | null>(null);
  // Command-mode indicator: the coordinator emits command-modifier-changed at
  // engage/release (and clears it at session end), so the pill shows a subtle
  // CMD chip while speech edits the buffer instead of dictating text.
  const [cmdActive, setCmdActive] = useState(false);

  const smoothedLevelsRef = useRef<number[]>(Array(16).fill(0));
  // Live-text scroll-back: the text region "sticks" to the newest line while the
  // user is at the bottom; if they scroll up to read history, auto-follow pauses
  // until they scroll back down.
  const capRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const direction = getLanguageDirection(i18n.language);

  useEffect(() => {
    const setupEventListeners = async () => {
      const unlistenShow = await listen("show-overlay", async (event) => {
        const overlayState = event.payload as OverlayState;
        // Reset synchronously before settings I/O. A fast microphone can emit
        // recording-ready while the awaits below are in flight; resetting after
        // them would overwrite that event and leave the overlay stuck arming.
        if (
          overlayState === "recording" ||
          overlayState === "streaming" ||
          overlayState === "preview"
        ) {
          setCaptureReady(false);
          smoothedLevelsRef.current = Array(16).fill(0);
          setLevels(Array(WAVE_BARS).fill(0));
          setStreamText({ committed: "", tentative: "" });
          setRemovedText(null);
          setNotice(null);
          setCmdActive(false);
          if (noticeTimerRef.current !== null) {
            window.clearTimeout(noticeTimerRef.current);
            noticeTimerRef.current = null;
          }
        }

        // Paint immediately from the cached language + placement (applied
        // at boot and reconciled on mount below); the background reconcile
        // that follows swaps in fresh values if the backend disagrees. Two
        // awaited IPC round trips used to sit here, gating every press.
        setState(overlayState);
        if (overlayState === "streaming" || overlayState === "preview") {
          setPhase("listening");
          setWorkKind("transcribing");
          setElapsed(0);
          setSession((s) => s + 1); // remount the card fresh for this session
        }
        setIsVisible(true);

        // Now that the overlay is visible, reconcile language + placement
        // with the backend in the background (no awaited IPC before paint).
        void reconcileFromSettings();
      });

      const unlistenHide = await listen("hide-overlay", () => {
        setIsVisible(false);
        setCaptureReady(false);
        setNotice(null);
        if (noticeTimerRef.current !== null) {
          window.clearTimeout(noticeTimerRef.current);
          noticeTimerRef.current = null;
        }
      });

      const unlistenReady = await listen("recording-ready", () => {
        setElapsed(0);
        setCaptureReady(true);
      });

      const unlistenLevel = await listen<number[]>("mic-level", (event) => {
        const newLevels = event.payload as number[];
        // Exponential smoothing across the 16 buckets, then take the first N
        // bars for the shared waveform.
        const smoothed = smoothedLevelsRef.current.map((prev, i) => {
          const target = newLevels[i] || 0;
          return prev * 0.7 + target * 0.3;
        });
        smoothedLevelsRef.current = smoothed;
        setLevels(smoothed.slice(0, WAVE_BARS));
      });

      const unlistenStream = await events.streamTextEvent.listen((event) => {
        setStreamText(event.payload);
        // A deletion report rides the same event as the refreshed text;
        // show it briefly, then clear. Ordinary ticks carry no report and
        // leave a still-visible chip alone.
        const deleted = event.payload.deleted;
        if (deleted) {
          setRemovedText(deleted);
          if (removedTimerRef.current !== null) {
            window.clearTimeout(removedTimerRef.current);
          }
          removedTimerRef.current = window.setTimeout(() => {
            setRemovedText(null);
            removedTimerRef.current = null;
          }, 1500);
        }
      });

      const unlistenPhase = await events.streamPhaseEvent.listen((event) => {
        const payload: StreamPhaseEvent = event.payload;
        setPhase(payload.phase);
        if (payload.kind) setWorkKind(payload.kind);
      });

      const unlistenNotice = await events.overlayNoticeEvent.listen((event) => {
        setNotice(event.payload);
        if (noticeTimerRef.current !== null) {
          window.clearTimeout(noticeTimerRef.current);
        }
        noticeTimerRef.current = window.setTimeout(() => {
          setNotice(null);
          noticeTimerRef.current = null;
        }, NOTICE_DISMISS_MS);
      });

      const unlistenCmd = await listen<boolean>(
        "command-modifier-changed",
        (event) => {
          setCmdActive(Boolean(event.payload));
        },
      );

      // Follow settings changes live: a language or overlay-position switch
      // in the settings window lands here immediately (and refreshes the
      // boot cache) instead of waiting for the next hotkey press.
      const unlistenSettings = await listen<{ setting?: string }>(
        "settings-changed",
        () => {
          void reconcileFromSettings();
        },
      );

      // Boot-order race cover: the cache seeded the first paint, now check
      // what the backend actually has.
      void reconcileFromSettings();

      return () => {
        unlistenShow();
        unlistenHide();
        unlistenReady();
        unlistenLevel();
        unlistenStream();
        unlistenPhase();
        unlistenNotice();
        unlistenCmd();
        unlistenSettings();
        // Never leave the removal chip's timer running past unmount.
        if (removedTimerRef.current !== null) {
          window.clearTimeout(removedTimerRef.current);
          removedTimerRef.current = null;
        }
        if (noticeTimerRef.current !== null) {
          window.clearTimeout(noticeTimerRef.current);
          noticeTimerRef.current = null;
        }
      };
    };

    setupEventListeners();
  }, [reconcileFromSettings]);

  // Elapsed capture timer starts only once microphone samples are flowing.
  useEffect(() => {
    if (state !== "streaming" || !isVisible || !captureReady) return;
    const id = setInterval(() => setElapsed((e) => e + 1), 1000);
    return () => clearInterval(id);
  }, [state, isVisible, captureReady]);

  // Stick to the bottom as text streams in - but only while pinned, so a user who
  // has scrolled up to read history isn't yanked back down by the next chunk.
  useLayoutEffect(() => {
    const el = capRef.current;
    if (!el) return;
    // Fade the top edge only once text actually overflows the cap.
    setOverflowing(el.scrollHeight > el.clientHeight + 1);
    if (pinnedRef.current) el.scrollTop = el.scrollHeight;
  }, [streamText]);

  // Each fresh streaming session starts pinned to the bottom, fade cleared.
  useEffect(() => {
    pinnedRef.current = true;
    setOverflowing(false);
  }, [session]);

  if (!isVisible) return null;

  // Re-pin when the user is within ~a line of the bottom; unpin otherwise.
  const handleStreamScroll = () => {
    const el = capRef.current;
    if (!el) return;
    pinnedRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= 16;
  };

  const fmtTime = (s: number) =>
    `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;

  // ---- Shared building blocks (one visual language for every overlay form) ----
  const waveform = (
    <div className={`swave ${captureReady ? "ready" : "arming"}`}>
      {levels.map((v, i) => (
        <i
          key={i}
          style={{
            height: `${Math.max(3, Math.min(18, 3 + Math.pow(v, 0.7) * 15))}px`,
          }}
        />
      ))}
    </div>
  );

  const cancelBtn = (
    <button
      className="sx"
      aria-label="cancel"
      onClick={() => commands.cancelOperation()}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true">
        <path
          d="M4 4 L12 12 M12 4 L4 12"
          stroke="currentColor"
          strokeWidth="1.6"
          strokeLinecap="round"
        />
      </svg>
    </button>
  );

  // dot (left) | waveform (center) | timer + cancel (right) - same structure for
  // pill & panel, so the Live morph is a pure width change.
  const listeningRow = (showTimer: boolean, showCancel: boolean) => (
    <div className="sbase">
      <div className="sbase-l">
        <span className={`sdot ${captureReady ? "ready" : "arming"}`} />
        {cmdActive && (
          <span className="scmd" aria-label={t("overlay.commandBadge")}>
            CMD
          </span>
        )}
      </div>
      {waveform}
      <div className="sbase-r">
        {showTimer && <span className="stimer">{fmtTime(elapsed)}</span>}
        {showCancel && cancelBtn}
      </div>
    </div>
  );

  // spinner (left) | label (center) | cancel (right) - same 3-zone grid as the
  // listening row, so the label is centered.
  const workingRow = (label: string, showCancel: boolean) => (
    <div className="sbase">
      <div className="sbase-l">
        <span className="sspinner" />
      </div>
      <span className="swork-label">{label}</span>
      <div className="sbase-r">{showCancel && cancelBtn}</div>
    </div>
  );

  // The notice strip: a failure or fallback the backend surfaced, shown only
  // while the card is visible (see the notice state comment above). Styled on
  // the removal chip; the error variant carries the app's one red treatment.
  const noticeRow = notice !== null && (
    <div className={`snotice ${noticeTone(notice)}`} role="status">
      {noticeMessage(notice, t)}
    </div>
  );

  // ---- Live overlay: a pill that sculpts open into a panel ----
  // The final-text preview ("preview") reuses the Live card: it shows the
  // finished transcription briefly before the paste fires, so batch models
  // get the same final confirmation streaming models already had.
  if (state === "streaming" || state === "preview") {
    const isPreview = state === "preview";
    const hasText =
      streamText.committed.length > 0 || streamText.tentative.length > 0;
    const working = phase === "working";
    // Keep the panel open whenever there's text - even while finalizing - so the
    // transcript stays put under a working spinner instead of collapsing and
    // squishing the text mid-stream. Only fall back to the small working pill
    // when there was no text to preserve.
    const open = hasText;
    const collapsed = working && !hasText;

    return (
      <div dir={direction} className={`ov-stage ${position}`}>
        <div
          key={session}
          className={`scard ${open ? "open" : ""} ${collapsed ? "working" : ""} ${
            isVisible ? "" : "leaving"
          } ${notice !== null ? "has-notice" : ""}`}
        >
          <div className="stext">
            <div className="stext-clip">
              <div
                className={`stext-cap ${overflowing ? "overflowing" : ""}`}
                ref={capRef}
                onScroll={handleStreamScroll}
              >
                <p>
                  <span className="committed">
                    {streamText.committed ? streamText.committed + " " : ""}
                  </span>
                  <span className="tentative">{streamText.tentative}</span>
                  {/* Drop the blinking caret once finalizing - it's no longer
                      capturing, and a static spinner conveys the work. The
                      preview keeps it: the text is final but not yet pasted. */}
                  {(!working || isPreview) && <span className="scaret" />}
                </p>
                {removedText !== null && (
                  <span className="sremoved" role="status">
                    <s>{t("overlay.removedWord", { text: removedText })}</s>
                  </span>
                )}
              </div>
            </div>
          </div>
          {working
            ? workingRow(
                workKind === "polishing"
                  ? t("overlay.processing")
                  : t("overlay.transcribing"),
                true,
              )
            : // The preview row keeps the shared 3-zone layout but drops the
              // elapsed timer: nothing is being timed anymore.
              listeningRow(open && !isPreview, true)}
          {noticeRow}
        </div>
      </div>
    );
  }

  // ---- Minimal overlay: exactly one row at a time - waveform (recording), or a
  // spinner + label (transcribing / processing). Never both. The pill animates its
  // width between them; the cancel button is in both rows so it stays put.
  const working = state === "transcribing" || state === "processing";
  const workLabel =
    state === "processing"
      ? t("overlay.processing")
      : t("overlay.transcribing");

  return (
    <div
      dir={direction}
      className={`ov-stage ${position} ov-fade ${isVisible ? "show" : ""}`}
    >
      <div
        className={`scard compact ${working && isVisible ? "cworking" : ""} ${
          notice !== null ? "has-notice" : ""
        }`}
      >
        {working ? workingRow(workLabel, true) : listeningRow(false, true)}
        {noticeRow}
      </div>
    </div>
  );
};

export default RecordingOverlay;
