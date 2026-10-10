import React from "react";
import { useTranslation } from "react-i18next";
import {
  ClipboardPaste,
  Cog,
  FlaskConical,
  History,
  Info,
  Sparkles,
  Cpu,
  Terminal,
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
    <div className="flex flex-col w-40 h-full border-e border-mid-gray/20 items-center px-2">
      <VoxBarLogo width={96} variant="compact" className="my-4" />
      <div className="flex flex-col w-full items-center gap-1 pt-2 border-t border-mid-gray/20">
        {availableSections.map((section) => {
          const Icon = section.icon;
          const isActive = section.enabledNow && activeSection === section.id;

          return (
            <div
              key={section.id}
              className={`flex gap-2 items-center p-2 w-full rounded-lg transition-colors ${
                isActive
                  ? "bg-logo-primary/80"
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
                  <p className="text-xs leading-tight text-secondary">
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
