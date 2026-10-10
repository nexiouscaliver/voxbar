import React, { Suspense, lazy, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "../../ui/Button";
import { SettingContainer } from "../../ui/SettingContainer";
import { findLatestReleaseNote } from "../../whats-new/releaseNotes";
import type { ReleaseNote } from "../../whats-new/releaseNotes";

// Lazy for the same reason as WhatsNewGate: this static import was pulling
// the modal (and react-markdown) into the critical bundle even though the
// modal only renders on a deliberate preview click.
const WhatsNewModal = lazy(() =>
  import("../../whats-new/WhatsNewModal").then((module) => ({
    default: module.WhatsNewModal,
  })),
);

interface WhatsNewPreviewProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const WhatsNewPreview: React.FC<WhatsNewPreviewProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const [note, setNote] = useState<ReleaseNote | null>(null);
  const [isLoading, setIsLoading] = useState(false);

  const preview = () => {
    setIsLoading(true);

    try {
      const releaseNote = findLatestReleaseNote();

      if (!releaseNote) {
        toast.info(t("settings.debug.whatsNewPreview.noNotes"));
        return;
      }

      setNote(releaseNote);
    } catch (error) {
      console.error("Failed to preview release notes:", error);
      toast.error(t("settings.debug.whatsNewPreview.error"));
    } finally {
      setIsLoading(false);
    }
  };

  return (
    <>
      <SettingContainer
        title={t("settings.debug.whatsNewPreview.title")}
        description={t("settings.debug.whatsNewPreview.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Button
          variant="secondary"
          size="md"
          onClick={preview}
          disabled={isLoading}
        >
          {t("settings.debug.whatsNewPreview.button")}
        </Button>
      </SettingContainer>

      {note && (
        <Suspense fallback={null}>
          <WhatsNewModal
            note={note}
            open={true}
            onDismiss={() => setNote(null)}
          />
        </Suspense>
      )}
    </>
  );
};
