import React, { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Search } from "lucide-react";
import { Input } from "../ui/Input";
import { DROPDOWN_PANEL_CLASS } from "../ui/Dropdown";
import { SECTIONS_CONFIG, type SidebarSection } from "../Sidebar";
import { useSettings } from "../../hooks/useSettings";
import { ALL_COMMAND_IDS } from "./commands/commandGroups";
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

interface SearchEntry {
  section: SidebarSection;
  key: string;
}

/**
 * Settings-wide search (UX research rec 6): a static index maps every
 * searchable label, the group titles and setting names each tab hosts plus
 * all 30 command display names, to the section that owns it. Typing filters
 * on the translated labels; picking a match jumps to that section. Kept
 * deliberately minimal: section-level jumps only, no per-row deep links.
 */
const STATIC_INDEX: readonly SearchEntry[] = [
  // General
  { section: "general", key: "sidebar.general" },
  { section: "general", key: "settings.general.title" },
  { section: "general", key: "settings.general.shortcut.title" },
  {
    section: "general",
    key: "settings.general.shortcut.bindings.transcribe.name",
  },
  { section: "general", key: "settings.general.shortcut.bindings.cancel.name" },
  { section: "general", key: "settings.general.shortcutActivation.title" },
  { section: "general", key: "settings.general.editingShortcuts.title" },
  {
    section: "general",
    key: "settings.general.editingShortcuts.deleteLastWord.title",
  },
  { section: "general", key: "settings.general.editingShortcuts.undo.title" },
  { section: "general", key: "settings.general.commandMode.title" },
  { section: "general", key: "settings.general.commandMode.toggle.title" },
  { section: "general", key: "settings.general.groups.app" },
  { section: "general", key: "settings.advanced.startHidden.label" },
  { section: "general", key: "settings.advanced.autostart.label" },
  { section: "general", key: "settings.advanced.showTrayIcon.label" },
  { section: "general", key: "settings.advanced.overlay.position.title" },
  { section: "general", key: "settings.advanced.overlay.style.title" },
  { section: "general", key: "settings.general.language.title" },
  { section: "general", key: "settings.advanced.translateToEnglish.label" },
  { section: "general", key: "settings.sound.title" },
  { section: "general", key: "settings.sound.microphone.title" },
  { section: "general", key: "settings.sound.channel.title" },
  { section: "general", key: "settings.debug.muteWhileRecording.label" },
  { section: "general", key: "settings.sound.audioFeedback.label" },
  { section: "general", key: "settings.debug.soundTheme.label" },
  { section: "general", key: "settings.sound.outputDevice.title" },
  { section: "general", key: "settings.sound.volume.title" },
  { section: "general", key: "settings.general.groups.microphone" },
  { section: "general", key: "settings.debug.alwaysOnMicrophone.label" },
  { section: "general", key: "settings.debug.clamshellMicrophone.title" },

  // Commands
  { section: "commands", key: "sidebar.commands" },
  { section: "commands", key: "settings.commands.behavior" },
  { section: "commands", key: "settings.advanced.autoInterpret.title" },
  { section: "commands", key: "settings.advanced.spokenPunctuation.title" },
  { section: "commands", key: "settings.advanced.numberFormat.title" },
  { section: "commands", key: "settings.advanced.terminalPunctuation.title" },
  { section: "commands", key: "settings.advanced.voiceDeletion.title" },
  { section: "commands", key: "settings.commands.groups.punctuation" },
  { section: "commands", key: "settings.commands.groups.symbols" },
  { section: "commands", key: "settings.commands.groups.editing" },
  { section: "commands", key: "settings.commands.groups.control" },

  // History
  { section: "history", key: "sidebar.history" },
  { section: "history", key: "settings.history.groups.retention" },
  { section: "history", key: "settings.debug.historyLimit.title" },
  { section: "history", key: "settings.debug.recordingRetention.title" },
  { section: "history", key: "settings.history.showModel.label" },
  { section: "history", key: "settings.history.title" },

  // Models
  { section: "models", key: "sidebar.models" },
  { section: "models", key: "settings.models.title" },
  { section: "models", key: "settings.models.yourModels" },
  { section: "models", key: "settings.models.availableModels" },
  { section: "models", key: "settings.models.addFromHf" },

  // Output
  { section: "output", key: "sidebar.output" },
  { section: "output", key: "settings.output.groups.paste" },
  { section: "output", key: "settings.advanced.pasteMethod.title" },
  { section: "output", key: "settings.advanced.typingTool.title" },
  { section: "output", key: "settings.advanced.clipboardHandling.title" },
  { section: "output", key: "settings.advanced.autoSubmit.title" },
  { section: "output", key: "settings.advanced.previewBeforePaste.title" },
  { section: "output", key: "settings.debug.appendTrailingSpace.label" },
  { section: "output", key: "settings.debug.reliablePaste.title" },
  { section: "output", key: "settings.debug.pasteDelay.title" },
  { section: "output", key: "settings.debug.pasteDelayAfter.title" },
  { section: "output", key: "settings.output.groups.recording" },
  { section: "output", key: "settings.debug.holdThreshold.title" },
  { section: "output", key: "settings.debug.recordingBuffer.title" },
  { section: "output", key: "settings.debug.streamingTail.title" },

  // Advanced
  { section: "advanced", key: "sidebar.advanced" },
  { section: "advanced", key: "settings.advanced.groups.model" },
  { section: "advanced", key: "settings.advanced.modelUnload.title" },
  { section: "advanced", key: "settings.advanced.memoryPressureGuard.label" },
  { section: "advanced", key: "settings.advanced.memoryHeadroom.label" },
  { section: "advanced", key: "settings.advanced.autoFallback.label" },
  { section: "advanced", key: "settings.advanced.menuBarModelTitle.label" },
  { section: "advanced", key: "settings.advanced.experimentalToggle.label" },
  { section: "advanced", key: "settings.advanced.groups.transcription" },
  {
    section: "advanced",
    key: "settings.advanced.voiceActivityDetection.title",
  },
  { section: "advanced", key: "settings.advanced.fillerWordRemoval.title" },
  { section: "advanced", key: "settings.advanced.chineseScript.title" },
  { section: "advanced", key: "settings.advanced.customWords.title" },
  {
    section: "advanced",
    key: "settings.debug.wordCorrectionThreshold.title",
  },
  { section: "advanced", key: "settings.advanced.groups.experimental" },
  { section: "advanced", key: "settings.debug.postProcessingToggle.label" },
  {
    section: "advanced",
    key: "settings.debug.keyboardImplementation.title",
  },
  { section: "advanced", key: "settings.advanced.acceleration.ort.title" },
  {
    section: "advanced",
    key: "settings.advanced.acceleration.transcribe.title",
  },
  { section: "advanced", key: "settings.advanced.lazyStreamClose.label" },
  { section: "advanced", key: "settings.advanced.vadBackend.title" },

  // Post Processing (gated on the post_process_enabled toggle)
  { section: "postprocessing", key: "sidebar.postProcessing" },
  { section: "postprocessing", key: "settings.postProcessing.hotkey.title" },
  { section: "postprocessing", key: "settings.postProcessing.api.title" },
  { section: "postprocessing", key: "settings.postProcessing.prompts.title" },

  // Debug (gated on debug_mode)
  { section: "debug", key: "sidebar.debug" },
  { section: "debug", key: "settings.debug.logLevel.title" },
  { section: "debug", key: "settings.debug.whatsNewPreview.title" },
  { section: "debug", key: "settings.debug.onboardingPreview.title" },
  { section: "debug", key: "settings.debug.keyboardDiagnostic.title" },
  { section: "debug", key: "settings.debug.liveLogs.title" },

  // About
  { section: "about", key: "sidebar.about" },
  { section: "about", key: "settings.about.title" },
  { section: "about", key: "settings.about.version.title" },
  { section: "about", key: "settings.about.whatsNewUpdates.label" },
  { section: "about", key: "settings.debug.updateChecks.label" },
  { section: "about", key: "settings.about.sourceCode.title" },
  { section: "about", key: "settings.about.appDataDirectory.title" },
  { section: "about", key: "settings.debug.logDirectory.title" },
  { section: "about", key: "settings.about.acknowledgments.title" },
];

const COMMAND_NAME_ENTRIES: readonly SearchEntry[] = ALL_COMMAND_IDS.map(
  (id) => ({
    section: "commands" as const,
    key: `settings.commands.names.${id}`,
  }),
);

const SEARCH_INDEX: readonly SearchEntry[] = [
  ...STATIC_INDEX,
  ...COMMAND_NAME_ENTRIES,
];

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
