// The five-bar VoxBar mark as a small nav icon. It fills with currentColor
// so it always reads on every surface the sidebar places it on: in an
// unselected row that is the theme text color (inherited), and inside the
// selected pill it follows the row's on-accent content color (white label
// white mark) instead of pinning the theme text color, which on the solid
// light-theme accent fill dropped the mark to ~3.9:1.
const VoxBarMark = ({
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
    className="fill-current"
    xmlns="http://www.w3.org/2000/svg"
  >
    {/* VoxBar mark: five voice bars */}
    <rect x="16" y="42.5" width="14" height="50" rx="7" />
    <rect x="36" y="27.5" width="14" height="80" rx="7" />
    <rect x="56" y="12.5" width="14" height="110" rx="7" />
    <rect x="76" y="27.5" width="14" height="80" rx="7" />
    <rect x="96" y="42.5" width="14" height="50" rx="7" />
  </svg>
);

export default VoxBarMark;
