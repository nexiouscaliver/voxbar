import React, { useState, useEffect } from "react";

import ModelSelector from "../model-selector";
import UpdateChecker from "../update-checker";
import { fetchAppVersion } from "../../lib/utils/appVersion";

const Footer: React.FC = () => {
  const [version, setVersion] = useState("");

  useEffect(() => {
    let cancelled = false;
    void fetchAppVersion().then((appVersion) => {
      if (!cancelled) setVersion(appVersion);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="w-full border-t border-mid-gray/20 pt-3">
      <div className="flex justify-between items-center text-xs px-4 pb-3 text-text/60">
        <div className="flex items-center gap-4">
          <ModelSelector />
        </div>

        {/* Update Status */}
        <div className="flex items-center gap-1">
          <UpdateChecker />
          <span>•</span>
          {/* eslint-disable-next-line i18next/no-literal-string */}
          <span>v{version}</span>
        </div>
      </div>
    </div>
  );
};

export default Footer;
