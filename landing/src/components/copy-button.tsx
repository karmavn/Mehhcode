import { useEffect, useRef, useState } from "react"
import { Check, Copy, X } from "lucide-react"

import { cn } from "@/lib/utils"

interface CopyButtonProps {
  text: string
  label: string
  className?: string
}

let lastAnnouncement = ""

function legacyCopy(text: string): boolean {
  const previousFocus = document.activeElement as HTMLElement | null
  const field = Object.assign(document.createElement("textarea"), { value: text })
  field.setAttribute("readonly", "")
  field.style.cssText = "position:fixed;inset-block-start:0;opacity:0"
  document.body.append(field)
  field.select()
  try {
    return document.execCommand("copy")
  } finally {
    field.remove()
    previousFocus?.focus({ preventScroll: true })
  }
}

export function CopyButton({ text, label, className }: CopyButtonProps) {
  const [status, setStatus] = useState<"idle" | "copied" | "failed">("idle")
  const [announcement, setAnnouncement] = useState("")
  const timerRef = useRef<number | null>(null)

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current)
      }
    }
  }, [])

  const announce = (ok: boolean) => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current)
    }
    setStatus(ok ? "copied" : "failed")
    let msg = ok ? "Copied" : "Copy failed"
    if (msg === lastAnnouncement) {
      msg += "\u00A0"
    }
    lastAnnouncement = msg
    setAnnouncement("")
    requestAnimationFrame(() => setAnnouncement(msg))

    timerRef.current = window.setTimeout(() => {
      setStatus("idle")
      setAnnouncement("")
    }, 2000)
  }

  const handleCopy = async () => {
    if (!text.trim()) return
    try {
      await navigator.clipboard.writeText(text)
      announce(true)
    } catch {
      announce(legacyCopy(text))
    }
  }

  return (
    <span className="relative inline-flex items-center">
      <button
        type="button"
        onClick={handleCopy}
        aria-label={label}
        data-copied={status === "copied" ? "" : undefined}
        data-failed={status === "failed" ? "" : undefined}
        className={cn(
          "inline-flex min-h-8 min-w-8 cursor-pointer items-center justify-center rounded-lg border border-border bg-background/85 px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground",
          className
        )}
      >
        {status === "copied" ? (
          <Check className="size-3.5 text-primary" aria-hidden="true" />
        ) : status === "failed" ? (
          <X className="size-3.5 text-destructive" aria-hidden="true" />
        ) : (
          <Copy className="size-3.5" aria-hidden="true" />
        )}
      </button>
      <span
        role="status"
        className={cn(
          "pointer-events-none absolute -top-8 right-0 rounded-md border border-border bg-popover px-2 py-0.5 text-[11px] font-medium whitespace-nowrap text-popover-foreground shadow-sm transition-opacity duration-150",
          status === "idle" ? "opacity-0" : "opacity-100"
        )}
      >
        {announcement}
      </span>
    </span>
  )
}
