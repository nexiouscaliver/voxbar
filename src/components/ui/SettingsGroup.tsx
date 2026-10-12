import React from "react";

interface SettingsGroupProps {
  title?: string;
  description?: string;
  children: React.ReactNode;
}

export const SettingsGroup: React.FC<SettingsGroupProps> = ({
  title,
  description,
  children,
}) => {
  return (
    <div className="space-y-2">
      {title && (
        <div className="px-4">
          {/* W1-L9 visual pass: group headers carry semibold text-text/70 so
              section boundaries read at a glance; they were secondary grey
              at the same weight as row descriptions. */}
          <h2 className="text-xs font-semibold text-text/70 uppercase tracking-wide">
            {title}
          </h2>
          {description && (
            <p className="text-xs text-secondary mt-1">{description}</p>
          )}
        </div>
      )}
      <div className="bg-background border border-mid-gray/20 rounded-lg overflow-visible">
        <div className="divide-y divide-mid-gray/20">{children}</div>
      </div>
    </div>
  );
};
