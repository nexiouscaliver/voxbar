import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

export interface DropdownOption {
  value: string;
  label: string;
  description?: string;
  disabled?: boolean;
}

/*
 * The one dropdown chrome, shared by every menu that pops from a control
 * (this component, the settings search results, the language pickers).
 * Panels are rounded-lg with a hairline border and the app shadow; rows are
 * rounded-lg chips inset from the panel edge so the hover/selected fill
 * reads as a row, not a stripe.
 */
export const DROPDOWN_PANEL_CLASS =
  "absolute bg-background border border-mid-gray/20 rounded-lg shadow-lg z-50 overflow-y-auto";

export const DROPDOWN_ROW_CLASS =
  "w-full text-start text-sm rounded-lg mx-1 my-0.5 px-2 py-1.5 transition-colors duration-150";

interface DropdownProps {
  options: DropdownOption[];
  className?: string;
  menuClassName?: string;
  selectedValue: string | null;
  onSelect: (value: string) => void;
  placeholder?: string;
  disabled?: boolean;
  onOpen?: () => void;
}

export const Dropdown: React.FC<DropdownProps> = ({
  options,
  selectedValue,
  onSelect,
  className = "",
  menuClassName,
  placeholder = "Select an option...",
  disabled = false,
  onOpen,
}) => {
  const { t } = useTranslation();
  const [isOpen, setIsOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      if (
        dropdownRef.current &&
        !dropdownRef.current.contains(event.target as Node)
      ) {
        setIsOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, []);

  const selectedOption = options.find(
    (option) => option.value === selectedValue,
  );

  const handleSelect = (value: string) => {
    onSelect(value);
    setIsOpen(false);
  };

  const handleToggle = () => {
    if (disabled) return;
    if (!isOpen) onOpen?.();
    setIsOpen(!isOpen);
  };

  return (
    <div className={`relative ${className}`} ref={dropdownRef}>
      <button
        type="button"
        className={`px-2 py-[5px] text-sm font-medium bg-mid-gray/10 border border-mid-gray/20 rounded-lg min-w-[200px] w-full text-start grid grid-cols-[1fr_auto] gap-2 items-center transition-all duration-150 ${
          disabled
            ? "opacity-50 cursor-not-allowed"
            : "hover:bg-logo-primary/10 cursor-pointer hover:border-logo-primary"
        }`}
        onClick={handleToggle}
        disabled={disabled}
      >
        <span className="truncate">{selectedOption?.label || placeholder}</span>
        <svg
          className={`w-4 h-4 transition-transform duration-200 ${isOpen ? "transform rotate-180" : ""}`}
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
      {isOpen && !disabled && (
        <div
          className={`top-full mt-1 max-h-60 py-1 ${DROPDOWN_PANEL_CLASS} ${
            menuClassName ?? "left-0 right-0"
          }`}
        >
          {options.length === 0 ? (
            <div className="px-3 py-1.5 text-sm text-secondary">
              {t("common.noOptionsFound")}
            </div>
          ) : (
            options.map((option) => {
              const isSelected = selectedValue === option.value;
              return (
                <button
                  key={option.value}
                  type="button"
                  className={`${DROPDOWN_ROW_CLASS} ${
                    isSelected
                      ? // Selected rows fill with the solid accent and
                        // carry on-accent content (the Badge primary pair,
                        // contrast-tested >= 4.5:1 for every accent in both
                        // themes). The old logo-primary tint + logo-primary
                        // text fell to 1.63:1 in light theme. No hover tint
                        // on the solid row: it would put white text on a
                        // pale wash.
                        "bg-background-ui text-accent-on"
                      : "hover:bg-logo-primary/10"
                  } ${option.disabled ? "opacity-50 cursor-not-allowed" : ""}`}
                  onClick={() => handleSelect(option.value)}
                  disabled={option.disabled}
                >
                  <span
                    className={`block whitespace-normal break-words ${
                      option.description || isSelected ? "font-medium" : ""
                    }`}
                  >
                    {option.label}
                  </span>
                  {option.description && (
                    <span
                      className={`mt-0.5 block whitespace-normal text-xs font-normal leading-snug ${
                        isSelected ? "text-accent-on" : "text-secondary"
                      }`}
                    >
                      {option.description}
                    </span>
                  )}
                </button>
              );
            })
          )}
        </div>
      )}
    </div>
  );
};
