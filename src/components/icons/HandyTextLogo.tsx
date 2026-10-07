import React from "react";

const HandyTextLogo = ({
  width,
  height,
  className,
}: {
  width?: number;
  height?: number;
  className?: string;
}) => {
  return (
    <svg
      width={width}
      height={height}
      className={className}
      viewBox="0 0 930 328"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      {/* Neutral VoxBar mark: five voice bars */}
      <g className="logo-primary">
        <rect x="40" y="94" width="56" height="140" rx="28" />
        <rect x="120" y="64" width="56" height="200" rx="28" />
        <rect x="200" y="34" width="56" height="260" rx="28" />
        <rect x="280" y="64" width="56" height="200" rx="28" />
        <rect x="360" y="94" width="56" height="140" rx="28" />
      </g>
    </svg>
  );
};

export default HandyTextLogo;
