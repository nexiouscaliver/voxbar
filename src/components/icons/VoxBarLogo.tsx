import React from "react";

/**
 * The VoxBar lockup: the five-bar voice mark plus a typographic "VoxBar"
 * wordmark drawn as inline SVG text in the system font (no external
 * assets). Both halves fill through the existing `logo-primary` class, so
 * the light/dark palettes and the accent picker keep driving the whole
 * lockup. The brand name renders as-is, never through t().
 */

// The system font stack every VoxBar webview has: macOS first (this is a
// macOS app), then the common desktop fallbacks for browser previews.
const WORDMARK_FONT =
  "-apple-system, BlinkMacSystemFont, 'system-ui', 'Segoe UI', sans-serif";

// The brand name renders as-is, never through t().
const WORDMARK_TEXT = "VoxBar";

// The five voice bars of the mark, unchanged from the previous logo.
const VoiceBars = () => (
  <g className="logo-primary">
    <rect x="40" y="94" width="56" height="140" rx="28" />
    <rect x="120" y="64" width="56" height="200" rx="28" />
    <rect x="200" y="34" width="56" height="260" rx="28" />
    <rect x="280" y="64" width="56" height="200" rx="28" />
    <rect x="360" y="94" width="56" height="140" rx="28" />
  </g>
);

// The wordmark, centered in the reserved space so platform metric
// differences can never push it out of the lockup.
const Wordmark = ({
  centerX,
  baselineY,
  fontSize,
}: {
  centerX: number;
  baselineY: number;
  fontSize: number;
}) => (
  <text
    x={centerX}
    y={baselineY}
    textAnchor="middle"
    fontFamily={WORDMARK_FONT}
    fontSize={fontSize}
    fontWeight={700}
    letterSpacing={fontSize * 0.02}
    className="logo-primary"
  >
    {WORDMARK_TEXT}
  </text>
);

const VoxBarLogo = ({
  width,
  height,
  className,
  variant = "full",
}: {
  width?: number;
  height?: number;
  className?: string;
  variant?: "full" | "compact";
}) => {
  if (variant === "compact") {
    // Stacked lockup for narrow slots (sidebar): the mark on top, the
    // wordmark beneath. The baseline sits a cap-height plus a gap below
    // the bars so the two never collide at any scale.
    return (
      <svg
        width={width}
        height={height}
        className={className}
        viewBox="40 20 376 375"
        fill="none"
        xmlns="http://www.w3.org/2000/svg"
      >
        <VoiceBars />
        <Wordmark centerX={228} baselineY={373} fontSize={76} />
      </svg>
    );
  }

  return (
    <svg
      width={width}
      height={height}
      className={className}
      viewBox="0 0 930 328"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      <VoiceBars />
      {/* The 930-wide viewBox reserves the space right of the bars (x>416)
          for the wordmark; center the text in it. */}
      <Wordmark centerX={673} baselineY={212} fontSize={118} />
    </svg>
  );
};

export default VoxBarLogo;
