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
    // text-accent-on keeps labels legible on light accent fills (white /
    // light-gray renders of the monochrome accents); for the default pink it
    // resolves to the theme text color, matching the previous look.
    primary: "bg-logo-primary text-accent-on",
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
