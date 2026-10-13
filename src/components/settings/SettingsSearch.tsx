import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Search } from "lucide-react";
import { Input } from "../ui/Input";
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

  // KB-144: rows that only render inside Advanced's experimental group are
  // dead results while the flag is off, exactly like disabled sections.
  const experimentalEnabled = settings?.experimental_enabled ?? false;

  const results = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return [];
    // Dedupe on the RENDERED pair (label + owning section): the index
    // legitimately lists both a setting title and its group heading
    // ("Microphone" twice under General), and two pixel-identical rows
    // read as a rendering bug. The first indexed entry wins.
    const seen = new Set<string>();
    return SEARCH_INDEX.filter((entry) => {
      if (!sectionSearchable(entry.section)) return false;
      const label = t(entry.key);
      if (!label.toLowerCase().includes(needle)) return false;
      if (entry.requiresExperimental && !experimentalEnabled) return false;
      const dedupeKey = `${label}\u0000${t(SECTIONS_CONFIG[entry.section].labelKey)}`;
      if (seen.has(dedupeKey)) return false;
      seen.add(dedupeKey);
      return true;
    }).slice(0, MAX_RESULTS);
  }, [query, t, settings, experimentalEnabled]);

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

  // Bold the matched substring so the hit is scannable against the rest of
  // the label: the query the user typed, found wherever it landed.
  const highlightMatch = (label: string): React.ReactNode => {
    const needle = query.trim();
    if (!needle) return label;
    const at = label.toLowerCase().indexOf(needle.toLowerCase());
    if (at === -1) return label;
    return (
      <>
        {label.slice(0, at)}
        <strong className="font-semibold">
          {label.slice(at, at + needle.length)}
        </strong>
        {label.slice(at + needle.length)}
      </>
    );
  };

  return (
    <div ref={containerRef} className="relative w-full max-w-3xl">
      {/* The glyph lives inside the field: a relative wrapper with the
          magnifier absolutely positioned over the input's left inset, so the
          icon reads as part of the field instead of floating outside its
          border (the pattern the Models tab and every native macOS search
          field use). */}
      <div className="relative w-full">
        <Search
          className="pointer-events-none absolute start-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-text/40"
          aria-hidden="true"
        />
        <Input
          type="text"
          variant="compact"
          className="w-full ps-8"
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
          /* Elevated a step above the shared dropdown chrome: a raised
              surface (4% of the text color mixed into the background), a
              deeper shadow and a hairline ring, so the panel reads as
              floating over the pane instead of merging with the row
              beneath it in dark theme. */
          className="absolute inset-x-0 top-full z-20 overflow-y-auto rounded-lg border border-mid-gray/20 bg-[color-mix(in_srgb,var(--color-background),var(--color-text)_4%)] py-1 shadow-xl ring-1 ring-mid-gray/20"
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
                className={`flex w-full cursor-pointer items-baseline justify-between gap-3 rounded-lg mx-1 my-0.5 px-3 py-1.5 text-start text-sm transition-colors ${
                  index === selectedIndex
                    ? /* The keyboard/hover-active row carries the accent
                         treatment (tint + accent label) rather than the
                         faint neutral wash. */
                      "bg-logo-primary/10 text-logo-primary"
                    : "hover:bg-mid-gray/10"
                }`}
              >
                <span className="truncate">
                  {highlightMatch(t(result.key))}
                </span>
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
