import { useCallback, useEffect, useState } from "react"

export type ColorScheme = "light" | "dark"

const STORAGE_KEY = "mehh-theme"

function readStoredScheme(): ColorScheme {
  try {
    return localStorage.getItem(STORAGE_KEY) === "dark" ? "dark" : "light"
  } catch {
    return "light"
  }
}

function applyScheme(scheme: ColorScheme) {
  const meta = document.querySelector('meta[name="color-scheme"]')
  if (meta) {
    meta.setAttribute("content", scheme)
  }
  document.documentElement.classList.toggle("dark", scheme === "dark")
}

export function useColorScheme() {
  const [scheme, setScheme] = useState<ColorScheme>(() => readStoredScheme())

  useEffect(() => {
    applyScheme(scheme)
  }, [scheme])

  useEffect(() => {
    const onStorage = (event: StorageEvent) => {
      if (event.key !== STORAGE_KEY) return
      const next: ColorScheme = event.newValue === "dark" ? "dark" : "light"
      setScheme(next)
    }
    window.addEventListener("storage", onStorage)
    return () => window.removeEventListener("storage", onStorage)
  }, [])

  const setColorScheme = useCallback((next: ColorScheme) => {
    try {
      localStorage.setItem(STORAGE_KEY, next)
    } catch {
      // Ignore storage write errors in restricted browsing contexts.
    }
    applyScheme(next)
    setScheme(next)
  }, [])

  return { scheme, setColorScheme }
}
