import React from "react";

type ModelStatus =
  | "ready"
  | "loading"
  | "downloading"
  | "verifying"
  | "extracting"
  | "error"
  | "unloaded"
  | "none";

interface ModelStatusButtonProps {
  status: ModelStatus;
  /** Localized one-line status word (e.g. "Model Ready") shown beside the
   * model name in the tooltip/accessible label, so the dot's color is never
   * the only carrier of the state (WCAG 1.4.1). */
  statusLabel: string;
  displayText: string;
  isDropdownOpen: boolean;
  onClick: () => void;
  className?: string;
}

const ModelStatusButton: React.FC<ModelStatusButtonProps> = ({
  status,
  statusLabel,
  displayText,
  isDropdownOpen,
  onClick,
  className = "",
}) => {
  const getStatusColor = (status: ModelStatus): string => {
    switch (status) {
      case "ready":
        // The same semantic green the Debug Live Logs "Live" dot uses, so
        // one green means "running/ready" everywhere in the app. The old
        // green-400 read dimmer than Live's emerald and the unloaded gray
        // dominated every resting screenshot.
        return "bg-emerald-500";
      case "loading":
        return "bg-yellow-400 animate-pulse";
      case "downloading":
        return "bg-logo-primary animate-pulse";
      case "verifying":
        return "bg-orange-400 animate-pulse";
      case "extracting":
        return "bg-orange-400 animate-pulse";
      case "error":
        return "bg-red-400";
      case "unloaded":
        return "bg-mid-gray/60";
      case "none":
        return "bg-red-400";
      default:
        return "bg-mid-gray/60";
    }
  };

  const accessibleLabel = `${statusLabel} · ${displayText}`;

  return (
    <button
      onClick={onClick}
      className={`flex items-center gap-2 hover:text-text/80 transition-colors ${className}`}
      title={accessibleLabel}
      aria-label={accessibleLabel}
    >
      <div className={`w-2 h-2 rounded-full ${getStatusColor(status)}`} />
      <span className="max-w-36 truncate">{displayText}</span>
      <svg
        className={`w-3 h-3 transition-transform ${isDropdownOpen ? "rotate-180" : ""}`}
        fill="none"
        stroke="currentColor"
        viewBox="0 0 24 24"
      >
        <path
          strokeLinecap="round"
          strokeLinejoin="round"
          strokeWidth={2}
          d="M19 9l-7 7-7-7"
        />
      </svg>
    </button>
  );
};

export default ModelStatusButton;
