import React from "react";

interface TextareaProps
  extends React.TextareaHTMLAttributes<HTMLTextAreaElement> {
  variant?: "default" | "compact";
}

export const Textarea: React.FC<TextareaProps> = ({
  className = "",
  variant = "default",
  ...props
}) => {
  const baseClasses =
    "px-2 py-1 text-sm font-medium bg-mid-gray/10 border border-mid-gray/80 rounded-md text-start transition-[background-color,border-color] duration-150 hover:bg-logo-primary/10 hover:border-logo-primary focus:outline-none focus:bg-logo-primary/10 focus:border-logo-primary resize-y";

  const variantClasses = {
    // Bottom padding carries an extra step beyond the top: a scrolled
    // textarea otherwise parks its last line hard against the bottom
    // border, with descenders (g, y, p) touching the edge and the row
    // reading as clipped mid-glyph.
    default: "px-3 pt-2 pb-3.5 min-h-[100px]",
    compact: "px-2 pt-1.5 pb-3 min-h-[80px]",
  };

  return (
    <textarea
      className={`${baseClasses} ${variantClasses[variant]} ${className}`}
      {...props}
    />
  );
};
