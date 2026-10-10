import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Search } from "lucide-react";
import { Input } from "../ui/Input";
import { DROPDOWN_PANEL_CLASS } from "../ui/Dropdown";
import { SECTIONS_CONFIG, type SidebarSection } from "../Sidebar";
import { useSettings } from "../../hooks/useSettings";
import { SEARCH_INDEX } from "./settingsSearchIndex";
import { searchKeyboardReducer, type SearchKey } from "./searchKeyboardNav";

const HANDLED_KEYS: readonly SearchKey[] = [
  "ArrowUp",
  "ArrowDown",
  "Enter",
  "Escape",
];

interface SettingsSearchProps {
  onSelect: (section: SidebarSection) => void;
}

// Settings-wide search (UX research rec 6): the searchable rows - group
// titles, setting names and all 30 command display names, each mapped to
// the section that owns it - live in pure-data form in
// settingsSearchIndex.ts, unit-testable without React. Typing filters on
// the translated labels; picking a match jumps to that section. Kept
// deliberately minimal: section-level jumps only, no per-row deep links.

const MAX_RESULTS = 12;

export const SettingsSearch: React.FC<SettingsSearchProps> = ({ onSelect }) => {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const [query, setQuery] = useState("");
  const [focused, setFocused] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);

  // Post Processing entries stay searchable even while the feature is off:
  // choosing one jumps to Advanced, where the enabling toggle always renders,
  // so search is never a dead end. Other gated sections (debug) stay hidden.
  const sectionSearchable = (section: SidebarSection) =>
    section === "postprocessing" || SECTIONS_CONFIG[section].enabled(settings);

  const results = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return [];
    return SEARCH_INDEX.filter(
      (entry) =>
        sectionSearchable(entry.section) &&
        t(entry.key).toLowerCase().includes(needle),
    ).slice(0, MAX_RESULTS);
  }, [query, t, settings]);

  // Fresh query, fresh highlight: always start on the first row.
  useEffect(() => {
    setSelectedIndex(0);
  }, [query]);

  // Close the dropdown on any click outside the search field.
  useEffect(() => {
    const onPointerDown = (event: MouseEvent) => {
      if (!containerRef.current?.contains(event.target as Node)) {
        setFocused(false);
      }
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
    };
  }, []);

  // A section whose feature is off resolves to the section hosting its
  // enabling control (Post Process -> Advanced), so a search hit always
  // lands the user next to the toggle they need.
  const resolveSection = (section: SidebarSection): SidebarSection =>
    SECTIONS_CONFIG[section].enabled(settings) ? section : "advanced";

  const choose = (section: SidebarSection) => {
    onSelect(resolveSection(section));
    setQuery("");
    setFocused(false);
  };

  const dropdownOpen = focused && query.trim().length > 0;

  return (
    <div ref={containerRef} className="relative w-full max-w-3xl">
      <div className="flex w-full items-center gap-2">
        <Search className="h-4 w-4 shrink-0 text-mid-gray" aria-hidden="true" />
        <Input
          type="text"
          variant="compact"
          className="w-full"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onFocus={() => setFocused(true)}
          onKeyDown={(event) => {
            const key = event.key as SearchKey;
            if (!HANDLED_KEYS.includes(key)) return;

            event.preventDefault();
            const outcome = searchKeyboardReducer(
              { selectedIndex, resultCount: results.length },
              key,
            );
            setSelectedIndex(outcome.selectedIndex);

            if (outcome.action === "choose") {
              const chosen = results[outcome.selectedIndex];
              if (chosen) choose(chosen.section);
            } else if (outcome.action === "close") {
              setQuery("");
              setFocused(false);
            }
          }}
          placeholder={t("settings.search.placeholder")}
          aria-label={t("settings.search.placeholder")}
        />
      </div>
      {dropdownOpen && (
        <div
          role="listbox"
          className={`top-full z-20 py-1 ${DROPDOWN_PANEL_CLASS} inset-x-0`}
        >
          {results.length === 0 ? (
            <p className="px-4 py-3 text-sm text-secondary">
              {t("settings.search.noResults")}
            </p>
          ) : (
            results.map((result, index) => (
              <button
                key={`${result.section}:${result.key}`}
                ref={
                  index === selectedIndex
                    ? (node) => node?.scrollIntoView({ block: "nearest" })
                    : undefined
                }
                type="button"
                role="option"
                aria-selected={index === selectedIndex}
                onMouseEnter={() => setSelectedIndex(index)}
                onClick={() => choose(result.section)}
                className={`flex w-full cursor-pointer items-baseline justify-between gap-3 rounded-lg mx-1 my-0.5 px-3 py-1.5 text-start text-sm transition-colors hover:bg-mid-gray/10 ${
                  index === selectedIndex ? "bg-mid-gray/10" : ""
                }`}
              >
                <span className="truncate">{t(result.key)}</span>
                <span className="shrink-0 text-xs text-secondary">
                  {t(SECTIONS_CONFIG[result.section].labelKey)}
                </span>
              </button>
            ))
          )}
        </div>
      )}
    </div>
  );
};
