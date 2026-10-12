import React from "react";
import { useTranslation } from "react-i18next";
import { Dialog } from "../ui";
import { Button } from "../ui/Button";
import { MarkdownContent } from "./MarkdownContent";
import type { ReleaseNote } from "./releaseNotes";

interface WhatsNewModalProps {
  note: ReleaseNote;
  open: boolean;
  onDismiss: () => void;
}

export const WhatsNewModal: React.FC<WhatsNewModalProps> = ({
  note,
  open,
  onDismiss,
}) => {
  const { t } = useTranslation();

  return (
    <Dialog
      open={open}
      title={t("whatsNew.title", { version: note.version })}
      closeLabel={t("common.close")}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onDismiss();
      }}
      // The footer button gives the modal a definite end: the body scrolls
      // (long notes clipped their last heading at the bottom edge with no
      // visible scrollbar), so the primary action must not live only at the
      // bottom of the scroll.
      footer={
        <Button onClick={onDismiss} variant="primary" size="md">
          {t("whatsNew.gotIt")}
        </Button>
      }
    >
      <MarkdownContent markdown={note.markdown} />
    </Dialog>
  );
};
