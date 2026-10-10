import React from "react";

interface BadgeProps {
  children: React.ReactNode;
  variant?: "primary" | "success" | "secondary";
  className?: string;
}

const Badge: React.FC<BadgeProps> = ({
  children,
  variant = "primary",
  className = "",
}) => {
  const variantClasses = {
    // Solid fills use the accent-for-fill token (background-ui) with
    // text-accent-on: the pair is chosen so labels pass 4.5:1 for every
    // accent. logo-primary is the render color for text/borders/tints, not a
    // text-bearing fill.
    primary: "bg-background-ui text-accent-on",
    success: "bg-green-500/20 text-green-400",
    secondary: "bg-mid-gray/20 text-text/70",
  };

  return (
    <span
      className={`inline-flex items-center px-3 py-1 rounded-full text-xs font-medium ${variantClasses[variant]} ${className}`}
    >
      {children}
    </span>
  );
};

export default Badge;
