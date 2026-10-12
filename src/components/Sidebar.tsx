import React from "react";
import { useTranslation } from "react-i18next";
import {
  Bell,
  ClipboardPaste,
  Cog,
  FlaskConical,
  History,
  Info,
  Sparkles,
  Cpu,
  Terminal,
  Wand2,
} from "lucide-react";
import VoxBarLogo from "./icons/VoxBarLogo";
import VoxBarMark from "./icons/VoxBarMark";
import { useSettings } from "../hooks/useSettings";
import {
  GeneralSettings,
  CommandsSettings,
  AdvancedSettings,
  HistorySettings,
  DebugSettings,
  AboutSettings,
  PostProcessingSettings,
  ModelsSettings,
  OutputSettings,
  NotificationsSettings,
  PromptStudio,
} from "./settings";

export type SidebarSection = keyof typeof SECTIONS_CONFIG;

interface IconProps {
  width?: number | string;
  height?: number | string;
  size?: number | string;
  className?: string;
  [key: string]: any;
}

interface SectionConfig {
  labelKey: string;
  icon: React.ComponentType<IconProps>;
  component: React.ComponentType;
  enabled: (settings: any) => boolean;
}

export const SECTIONS_CONFIG = {
  general: {
    labelKey: "sidebar.general",
    icon: VoxBarMark,
    component: GeneralSettings,
    enabled: () => true,
  },
  commands: {
    labelKey: "sidebar.commands",
    icon: Terminal,
    component: CommandsSettings,
    enabled: () => true,
  },
  history: {
    labelKey: "sidebar.history",
    icon: History,
    component: HistorySettings,
    enabled: () => true,
  },
  // v1.6.0 W1-L4: the OS-notification toggles plus the in-app notification
  // log live here; notification clicks on errors deep-navigate to this
  // section (App.tsx click-through wiring).
  notifications: {
    labelKey: "sidebar.notifications",
    icon: Bell,
    component: NotificationsSettings,
    enabled: () => true,
  },
  models: {
    labelKey: "sidebar.models",
    icon: Cpu,
    component: ModelsSettings,
    enabled: () => true,
  },
  output: {
    labelKey: "sidebar.output",
    icon: ClipboardPaste,
    component: OutputSettings,
    enabled: () => true,
  },
  advanced: {
    labelKey: "sidebar.advanced",
    icon: Cog,
    component: AdvancedSettings,
    enabled: () => true,
  },
  postprocessing: {
    labelKey: "sidebar.postProcessing",
    icon: Sparkles,
    component: PostProcessingSettings,
    enabled: (settings) => settings?.post_process_enabled ?? false,
  },
  // v1.6.0 W1-L6: the compare surface for prompt templates and engines.
  // Always enabled: the studio is the testing ground that helps decide
  // whether to turn post-processing on at all, so it must be reachable
  // while the feature itself is off (its runs never paste and never write
  // history).
  promptStudio: {
    labelKey: "sidebar.promptStudio",
    icon: Wand2,
    component: PromptStudio,
    enabled: () => true,
  },
  debug: {
    labelKey: "sidebar.debug",
    icon: FlaskConical,
    component: DebugSettings,
    enabled: (settings) => settings?.debug_mode ?? false,
  },
  about: {
    labelKey: "sidebar.about",
    icon: Info,
    component: AboutSettings,
    enabled: () => true,
  },
} as const satisfies Record<string, SectionConfig>;

interface SidebarProps {
  activeSection: SidebarSection;
  onSectionChange: (section: SidebarSection) => void;
}

export const Sidebar: React.FC<SidebarProps> = ({
  activeSection,
  onSectionChange,
}) => {
  const { t } = useTranslation();
  const { settings } = useSettings();

  // Post Processing stays in the nav even while its feature is off: the row
  // renders disabled with a hint and navigates to Advanced, where the
  // enabling toggle now always renders. Every other gated section (debug)
  // stays filtered out entirely.
  const availableSections = Object.entries(SECTIONS_CONFIG)
    .filter(
      ([id, config]) => config.enabled(settings) || id === "postprocessing",
    )
    .map(([id, config]) => ({
      id: id as SidebarSection,
      ...config,
      enabledNow: config.enabled(settings),
    }));

  return (
    <div className="flex flex-col w-52 h-full border-e border-mid-gray/20 items-center px-2">
      {/* The brand is pinned: an explicit height plus shrink-0 keep the
          column flex layout from squashing the lockup when the nav below is
          taller than the window (the defect where the logo+wordmark
          collapsed into a 16px waveform sliver on scrolled panes). */}
      <VoxBarLogo
        width={64}
        height={64}
        variant="compact"
        className="my-3 shrink-0"
      />
      {/* The nav never scrolls with the content pane: it owns its own
          scroll area (visible inset scrollbar) so only it moves when the
          section list outgrows the window, and never at the cost of the
          brand above. */}
      <div className="settings-scroll flex flex-col w-full items-center flex-1 min-h-0 gap-0.5 pt-2 border-t border-mid-gray/20 overflow-y-auto">
        {availableSections.map((section) => {
          const Icon = section.icon;
          const isActive = section.enabledNow && activeSection === section.id;

          return (
            <div
              key={section.id}
              className={`flex gap-2 items-center py-1.5 px-2 w-full shrink-0 rounded-lg transition-colors ${
                isActive
                  ? // The selected pill fills with the solid accent and
                    // carries on-accent content, matching Badge's primary
                    // variant: the fill/onFill pair is contrast-tested for
                    // every accent in both themes (>= 4.5:1), unlike the
                    // old logo-primary/80 tint where the 13px label fell to
                    // 3.03:1 on dark accent fills.
                    "bg-background-ui text-accent-on"
                  : "hover:bg-mid-gray/20 hover:opacity-100 opacity-85"
              } ${section.enabledNow ? "cursor-pointer" : "cursor-default"}`}
              aria-disabled={!section.enabledNow}
              onClick={() =>
                onSectionChange(
                  // A disabled Post Process row still goes somewhere useful:
                  // Advanced hosts the toggle that turns it on.
                  section.enabledNow ? section.id : "advanced",
                )
              }
            >
              <Icon
                width={24}
                height={24}
                className={`shrink-0 ${section.enabledNow ? "" : "opacity-50"}`}
              />
              <div className="flex flex-col min-w-0">
                <p
                  className={`text-sm font-medium truncate ${section.enabledNow ? "" : "opacity-50"}`}
                  title={
                    section.enabledNow
                      ? t(section.labelKey)
                      : `${t(section.labelKey)}: ${t("sidebar.enableInAdvancedHint")}`
                  }
                >
                  {t(section.labelKey)}
                </p>
                {!section.enabledNow && (
                  <p
                    className="text-xs leading-tight text-secondary truncate"
                    title={t("sidebar.enableInAdvancedHint")}
                  >
                    {t("sidebar.enableInAdvancedHint")}
                  </p>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
};
