import React from "react";

/**
 * Slim three-dot progress indicator for the onboarding flow (permissions,
 * confirmation, model pick). Purely decorative: the cards themselves carry
 * the state, so the dots stay aria-hidden.
 */
const StepDots: React.FC<{ active: 0 | 1 | 2 }> = ({ active }) => (
  <div className="flex items-center gap-1.5" aria-hidden="true">
    {[0, 1, 2].map((i) => (
      <span
        key={i}
        className={`w-1.5 h-1.5 rounded-full transition-colors ${
          i === active ? "bg-logo-primary" : "bg-mid-gray/40"
        }`}
      />
    ))}
  </div>
);

export default StepDots;
