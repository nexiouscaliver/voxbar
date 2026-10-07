const HandyHand = ({
  width,
  height,
}: {
  width?: number | string;
  height?: number | string;
}) => (
  <svg
    width={width || 126}
    height={height || 135}
    viewBox="0 0 126 135"
    className="fill-text"
    xmlns="http://www.w3.org/2000/svg"
  >
    {/* Neutral VoxBar mark: five voice bars */}
    <rect x="16" y="42.5" width="14" height="50" rx="7" />
    <rect x="36" y="27.5" width="14" height="80" rx="7" />
    <rect x="56" y="12.5" width="14" height="110" rx="7" />
    <rect x="76" y="27.5" width="14" height="80" rx="7" />
    <rect x="96" y="42.5" width="14" height="50" rx="7" />
  </svg>
);

export default HandyHand;
