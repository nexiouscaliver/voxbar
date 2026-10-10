import type { SidebarSection } from "../Sidebar";
import { ALL_COMMAND_IDS } from "./commands/commandGroups";

export interface SearchEntry {
  section: SidebarSection;
  key: string;
}

/**
 * Pure data for the settings-wide search (UX research rec 6): a static
 * index maps every searchable label, the group titles and setting names
 * each tab hosts plus all 30 command display names, to the section that
 * owns it. It lives here, apart from the SettingsSearch component, so it
 * is unit-testable without React (settingsSearchIndex.test.ts runs as a
 * plain bun script). The component filters these entries on the
 * translated labels; picking a match jumps to that section.
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

  // About. KB-190: rows indexed in AboutSettings tab order - App Language,
  // Update Policy, and (AUD-11) the Theme and Accent Color rows
  // (theme.title, theme.accentColor.label) are reachable by search.
  { section: "about", key: "sidebar.about" },
  { section: "about", key: "settings.about.title" },
  { section: "about", key: "appLanguage.title" },
  { section: "about", key: "theme.title" },
  { section: "about", key: "theme.accentColor.label" },
  { section: "about", key: "settings.about.version.title" },
  { section: "about", key: "settings.about.whatsNewUpdates.label" },
  { section: "about", key: "settings.debug.updateChecks.label" },
  { section: "about", key: "settings.about.updatePolicy.title" },
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

export const SEARCH_INDEX: readonly SearchEntry[] = [
  ...STATIC_INDEX,
  ...COMMAND_NAME_ENTRIES,
];
