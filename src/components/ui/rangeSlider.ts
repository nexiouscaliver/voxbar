/*
 * The one range-control recipe: a 6px track with an explicitly sized 14px
 * round thumb, so the native thumb (which renders ~10px+ at its default)
 * never towers over or clips out of a hairline track. Callers paint the
 * track fill inline with a linear-gradient; the thumb reads the
 * accent-for-fill token with a background-colored ring so it is visible on
 * both ends of the gradient in both themes.
 */
export const RANGE_INPUT_CLASS = [
  "h-1.5 rounded-full appearance-none cursor-pointer",
  "focus:outline-none focus:ring-2 focus:ring-logo-primary/50",
  "disabled:opacity-50 disabled:cursor-not-allowed",
  "[&::-webkit-slider-thumb]:appearance-none",
  "[&::-webkit-slider-thumb]:h-3.5 [&::-webkit-slider-thumb]:w-3.5",
  "[&::-webkit-slider-thumb]:rounded-full",
  "[&::-webkit-slider-thumb]:bg-background-ui",
  "[&::-webkit-slider-thumb]:border-2 [&::-webkit-slider-thumb]:border-background",
  "[&::-webkit-slider-thumb]:cursor-pointer",
  "[&::-moz-range-thumb]:h-3.5 [&::-moz-range-thumb]:w-3.5",
  "[&::-moz-range-thumb]:rounded-full",
  "[&::-moz-range-thumb]:bg-background-ui",
  "[&::-moz-range-thumb]:border-2 [&::-moz-range-thumb]:border-background",
  "[&::-moz-range-thumb]:cursor-pointer",
].join(" ");
