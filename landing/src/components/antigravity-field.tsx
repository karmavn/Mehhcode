import { useEffect, useRef } from "react"

import type { ColorScheme } from "@/lib/theme"

interface Capsule {
  homeX: number
  homeY: number
  x: number
  y: number
  vx: number
  vy: number
  angle: number
  baseAngle: number
  length: number
  width: number
  phase: number
  colorRgb: string
  baseAlpha: number
}

interface Pulse {
  x: number
  y: number
  r: number
  maxR: number
  alpha: number
}

const LIGHT_PALETTE = [
  "225, 29, 72",
  "217, 119, 87",
  "244, 63, 94",
  "234, 88, 12",
  "100, 116, 139",
] as const

const DARK_PALETTE = [
  "251, 113, 133",
  "217, 119, 87",
  "244, 63, 94",
  "251, 191, 36",
  "148, 163, 184",
] as const

const INFLUENCE_RADIUS = 230
const PULSE_KICK_RADIUS = 200
const MAX_PULSES = 4

function pickColor(palette: readonly string[], seed: number): string {
  const r = (((Math.sin(seed * 12.9898) * 43758.5453) % 1) + 1) % 1
  if (r < 0.34) return palette[0]
  if (r < 0.64) return palette[1]
  if (r < 0.82) return palette[2]
  if (r < 0.92) return palette[3]
  return palette[4]
}

function createCapsules(
  width: number,
  height: number,
  scheme: ColorScheme
): Capsule[] {
  const palette = scheme === "dark" ? DARK_PALETTE : LIGHT_PALETTE
  const baseAlpha = scheme === "dark" ? 0.26 : 0.24
  const targetCount = 156
  const aspect = width / Math.max(height, 1)
  const cols = Math.max(8, Math.round(Math.sqrt(targetCount * aspect)))
  const rows = Math.max(6, Math.round(targetCount / cols))
  const cellW = width / cols
  const cellH = height / rows
  const capsules: Capsule[] = []

  let idx = 0
  for (let row = 0; row < rows; row++) {
    for (let col = 0; col < cols; col++) {
      idx++
      const jx = ((((Math.sin(idx * 91.7) * 43758.5) % 1) + 1) % 1) - 0.5
      const jy = ((((Math.cos(idx * 47.3) * 24634.6) % 1) + 1) % 1) - 0.5
      const jAngle = (((Math.sin(idx * 19.1) * 19432.1) % 1) + 1) % 1
      const jSize = (((Math.cos(idx * 73.9) * 31987.4) % 1) + 1) % 1

      const homeX = (col + 0.5 + jx * 0.72) * cellW
      const homeY = (row + 0.5 + jy * 0.72) * cellH
      const baseAngle = jAngle * Math.PI * 2

      capsules.push({
        homeX,
        homeY,
        x: homeX,
        y: homeY,
        vx: 0,
        vy: 0,
        angle: baseAngle,
        baseAngle,
        length: 5 + jSize * 5.5,
        width: 1.8 + jSize * 1.0,
        phase: idx * 0.61,
        colorRgb: pickColor(palette, idx),
        baseAlpha,
      })
    }
  }

  return capsules
}

export function AntigravityField({ scheme }: { scheme: ColorScheme }) {
  const auraRef = useRef<HTMLDivElement>(null)
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const ctx = canvas.getContext("2d")
    if (!ctx) return

    const motionQuery = window.matchMedia("(prefers-reduced-motion: reduce)")
    const pulseStroke = scheme === "dark" ? "251, 113, 133" : "225, 29, 72"

    let width = window.innerWidth
    let height = window.innerHeight
    let dpr = Math.min(window.devicePixelRatio || 1, 2)
    let capsules = createCapsules(width, height, scheme)
    const pulses: Pulse[] = []
    let rafId = 0

    const pointer = {
      tx: width * 0.5,
      ty: height * 0.32,
      sx: width * 0.5,
      sy: height * 0.32,
      active: false,
    }

    const resize = () => {
      width = window.innerWidth
      height = window.innerHeight
      dpr = Math.min(window.devicePixelRatio || 1, 2)
      canvas.width = Math.floor(width * dpr)
      canvas.height = Math.floor(height * dpr)
      canvas.style.width = `${width}px`
      canvas.style.height = `${height}px`
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      capsules = createCapsules(width, height, scheme)
    }

    const drawCapsule = (c: Capsule, alpha: number, scaleLen: number) => {
      const half = c.length * scaleLen * 0.5
      const cos = Math.cos(c.angle)
      const sin = Math.sin(c.angle)
      ctx.beginPath()
      ctx.moveTo(c.x - cos * half, c.y - sin * half)
      ctx.lineTo(c.x + cos * half, c.y + sin * half)
      ctx.strokeStyle = `rgba(${c.colorRgb}, ${alpha.toFixed(3)})`
      ctx.lineWidth = c.width
      ctx.lineCap = "round"
      ctx.stroke()
    }

    const drawStaticFrame = () => {
      ctx.clearRect(0, 0, width, height)
      for (const c of capsules) {
        c.x = c.homeX
        c.y = c.homeY
        c.vx = 0
        c.vy = 0
        c.angle = c.baseAngle
        drawCapsule(c, c.baseAlpha, 1)
      }
    }

    const stopLoop = () => {
      if (rafId !== 0) {
        window.cancelAnimationFrame(rafId)
        rafId = 0
      }
    }

    const tick = (now: number) => {
      if (motionQuery.matches || document.hidden) {
        stopLoop()
        return
      }

      pointer.sx += (pointer.tx - pointer.sx) * 0.18
      pointer.sy += (pointer.ty - pointer.sy) * 0.18

      if (auraRef.current) {
        auraRef.current.style.setProperty("--mx", `${pointer.sx}px`)
        auraRef.current.style.setProperty("--my", `${pointer.sy}px`)
      }

      ctx.clearRect(0, 0, width, height)

      for (const c of capsules) {
        const floatX = c.homeX + Math.sin(now * 0.0012 + c.phase) * 3.5
        const floatY = c.homeY + Math.cos(now * 0.001 + c.phase) * 3.5

        let alpha = c.baseAlpha
        let scaleLen = 1
        let targetAngle = c.baseAngle + Math.sin(now * 0.0007 + c.phase) * 0.16

        if (pointer.active) {
          const dx = c.x - pointer.tx
          const dy = c.y - pointer.ty
          const d = Math.hypot(dx, dy)
          if (d < INFLUENCE_RADIUS) {
            const proximity = Math.pow(1 - d / INFLUENCE_RADIUS, 2)
            const nx = d > 0.001 ? dx / d : 1
            const ny = d > 0.001 ? dy / d : 0
            const repel = proximity * 2.35
            const swirl = proximity * 0.72
            c.vx += nx * repel - ny * swirl
            c.vy += ny * repel + nx * swirl

            targetAngle = Math.atan2(dy, dx) + 0.65
            alpha = c.baseAlpha + proximity * (0.86 - c.baseAlpha)
            scaleLen = 1 + proximity * 0.4
          }
        }

        let angleDiff = targetAngle - c.angle
        while (angleDiff > Math.PI) angleDiff -= Math.PI * 2
        while (angleDiff < -Math.PI) angleDiff += Math.PI * 2
        c.angle += angleDiff * 0.14

        c.vx += (floatX - c.x) * 0.048
        c.vy += (floatY - c.y) * 0.048
        c.vx *= 0.86
        c.vy *= 0.86
        c.x += c.vx
        c.y += c.vy

        drawCapsule(c, alpha, scaleLen)
      }

      for (let i = pulses.length - 1; i >= 0; i--) {
        const p = pulses[i]
        p.r += 9.5
        p.alpha *= 0.91
        if (p.r >= p.maxR || p.alpha <= 0.015) {
          pulses.splice(i, 1)
          continue
        }
        ctx.beginPath()
        ctx.arc(p.x, p.y, p.r, 0, Math.PI * 2)
        ctx.strokeStyle = `rgba(${pulseStroke}, ${p.alpha.toFixed(3)})`
        ctx.lineWidth = 1.25
        ctx.stroke()
      }

      rafId = window.requestAnimationFrame(tick)
    }

    const syncLoopState = () => {
      if (motionQuery.matches) {
        stopLoop()
        if (auraRef.current) {
          auraRef.current.style.display = "none"
        }
        pulses.length = 0
        drawStaticFrame()
        return
      }

      if (auraRef.current) {
        auraRef.current.style.display = ""
      }

      if (document.hidden) {
        stopLoop()
        return
      }

      if (rafId === 0) {
        rafId = window.requestAnimationFrame(tick)
      }
    }

    const onResize = () => {
      resize()
      if (motionQuery.matches) {
        drawStaticFrame()
      }
    }

    const onPointerMove = (e: PointerEvent) => {
      if (motionQuery.matches) return
      pointer.tx = e.clientX
      pointer.ty = e.clientY
      pointer.active = true
    }

    const onPointerLeave = () => {
      pointer.active = false
    }

    const onPointerDown = (e: PointerEvent) => {
      if (motionQuery.matches) return
      pointer.tx = e.clientX
      pointer.ty = e.clientY
      pointer.active = true

      if (pulses.length >= MAX_PULSES) {
        pulses.shift()
      }
      pulses.push({
        x: e.clientX,
        y: e.clientY,
        r: 6,
        maxR: 280,
        alpha: 0.42,
      })

      for (const c of capsules) {
        const dx = c.x - e.clientX
        const dy = c.y - e.clientY
        const dist = Math.hypot(dx, dy)
        if (dist > 0.5 && dist < PULSE_KICK_RADIUS) {
          const strength = (1 - dist / PULSE_KICK_RADIUS) * 5.2
          c.vx += (dx / dist) * strength
          c.vy += (dy / dist) * strength
        }
      }
    }

    resize()
    syncLoopState()

    window.addEventListener("resize", onResize)
    window.addEventListener("pointermove", onPointerMove, { passive: true })
    window.addEventListener("pointerleave", onPointerLeave)
    window.addEventListener("blur", onPointerLeave)
    window.addEventListener("pointerdown", onPointerDown, { passive: true })
    motionQuery.addEventListener("change", syncLoopState)
    document.addEventListener("visibilitychange", syncLoopState)

    return () => {
      stopLoop()
      window.removeEventListener("resize", onResize)
      window.removeEventListener("pointermove", onPointerMove)
      window.removeEventListener("pointerleave", onPointerLeave)
      window.removeEventListener("blur", onPointerLeave)
      window.removeEventListener("pointerdown", onPointerDown)
      motionQuery.removeEventListener("change", syncLoopState)
      document.removeEventListener("visibilitychange", syncLoopState)
    }
  }, [scheme])

  const auraGradient =
    scheme === "dark"
      ? "radial-gradient(480px circle at var(--mx, 50vw) var(--my, 32vh), rgba(251, 113, 133, 0.11), rgba(217, 119, 87, 0.06) 44%, transparent 74%)"
      : "radial-gradient(460px circle at var(--mx, 50vw) var(--my, 32vh), rgba(225, 29, 72, 0.085), rgba(217, 119, 87, 0.045) 42%, transparent 72%)"

  return (
    <div
      aria-hidden="true"
      className="antigravity-backdrop pointer-events-none fixed inset-0 z-[1] overflow-hidden"
    >
      <div
        ref={auraRef}
        className="absolute inset-0 transition-opacity duration-300"
        style={{ background: auraGradient }}
      />
      <canvas ref={canvasRef} className="size-full" />
    </div>
  )
}
