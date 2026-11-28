import { cn } from "@/lib/utils"
import type { ColorScheme } from "@/lib/theme"
import { AnimatedThemeToggler } from "@/components/ui/animated-theme-toggler"

interface ThemeToggleProps {
  scheme: ColorScheme
  onChangeScheme: (next: ColorScheme) => void
  className?: string
}

export function ThemeToggle({
  scheme,
  onChangeScheme,
  className,
}: ThemeToggleProps) {
  const isDark = scheme === "dark"

  return (
    <AnimatedThemeToggler
      variant="circle"
      theme={scheme}
      onThemeChange={onChangeScheme}
      aria-label={isDark ? "Switch to light mode" : "Switch to dark mode"}
      className={cn(
        "inline-flex size-9 cursor-pointer items-center justify-center rounded-full border border-border bg-card/85 text-foreground backdrop-blur-md transition-colors hover:bg-accent [&_svg]:size-4",
        className
      )}
    />
  )
}
