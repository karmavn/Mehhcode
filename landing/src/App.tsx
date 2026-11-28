import { useState } from "react"
import { ArrowRight, ArrowUpRight, Pause, Play, Terminal } from "lucide-react"
import { MotionConfig } from "motion/react"

import { AntigravityField } from "@/components/antigravity-field"
import { CopyButton } from "@/components/copy-button"
import { EcosystemSection } from "@/components/ecosystem-section"
import { EditorialFeatures } from "@/components/editorial-features"
import { ProductStage } from "@/components/product-stage"
import { ThemeToggle } from "@/components/theme-toggle"
import { BlurFade } from "@/components/ui/blur-fade"
import { Button } from "@/components/ui/button"
import { Marquee } from "@/components/ui/marquee"
import { ScrollProgress } from "@/components/ui/scroll-progress"
import { Separator } from "@/components/ui/separator"
import { ShimmerButton } from "@/components/ui/shimmer-button"
import { TypingAnimation } from "@/components/ui/typing-animation"
import {
  COMPANION_PROJECTS,
  MARQUEE_CAPABILITIES,
  MEHH_INSTALL_COMMAND,
  MEHH_REPO_URL,
} from "@/data/projects"
import { useColorScheme } from "@/lib/theme"

const QUICKSTART_STEPS = [
  {
    step: "01",
    title: "Install from source",
    command: MEHH_INSTALL_COMMAND,
    copyLabel: "Copy git clone and cargo install command",
  },
  {
    step: "02",
    title: "Authenticate (web_search is keyless)",
    command: "mehh --login anthropic",
    copyLabel: "Copy OAuth login command",
  },
  {
    step: "03",
    title: "Launch TUI with codemode",
    command: "mehh --codemode",
    copyLabel: "Copy launch command",
  },
]

export function App() {
  const { scheme, setColorScheme } = useColorScheme()
  const [motionPaused, setMotionPaused] = useState(false)

  return (
    <MotionConfig reducedMotion="user">
      <div className="relative min-h-screen bg-background text-foreground">
        <a href="#main-content" className="skip-link">
          Skip to main content
        </a>

        <ScrollProgress className="h-[2px] bg-linear-to-r from-primary via-[#fb7185] to-[#fda4af]" />

        {/* Luminous white + Gemini-inspired soft misty red backdrop */}
        <div
          aria-hidden="true"
          className="gemini-mist-backdrop pointer-events-none fixed inset-0 z-0"
        />

        {/* Full-viewport Antigravity cursor-reactive capsule field & aura */}
        <AntigravityField scheme={scheme} />

        {/* Top Navigation */}
        <header className="sticky top-0 z-40 border-b border-border/70 bg-background/80 backdrop-blur-xl">
          <div className="mx-auto flex h-16 max-w-6xl items-center justify-between gap-4 px-4 sm:px-6">
            <div className="flex items-center gap-7">
              <a
                href="#"
                className="flex items-center gap-2.5 rounded-lg text-sm font-semibold tracking-tight text-foreground"
              >
                <img
                  src="/karma-mark.png"
                  alt=""
                  width={358}
                  height={199}
                  className="h-6 w-auto"
                />
                <span>Mehhcode</span>
              </a>

              <nav
                aria-label="Primary"
                className="hidden items-center gap-6 text-sm text-muted-foreground md:flex"
              >
                <a
                  href="#capabilities"
                  className="transition-colors hover:text-foreground"
                >
                  Highlights
                </a>
                <a
                  href="#architecture"
                  className="transition-colors hover:text-foreground"
                >
                  Architecture
                </a>
                <a
                  href="#ecosystem"
                  className="transition-colors hover:text-foreground"
                >
                  Ecosystem
                </a>
                <a
                  href="#install"
                  className="transition-colors hover:text-foreground"
                >
                  Quickstart
                </a>
              </nav>
            </div>

            <div className="flex items-center gap-2.5">
              <ThemeToggle scheme={scheme} onChangeScheme={setColorScheme} />
              <Button
                size="sm"
                nativeButton={false}
                className="min-h-8 rounded-full bg-foreground px-4 text-background hover:bg-foreground/90"
                render={
                  <a
                    href={MEHH_REPO_URL}
                    target="_blank"
                    rel="noopener noreferrer"
                  />
                }
              >
                <span>GitHub</span>
                <ArrowUpRight data-icon="inline-end" aria-hidden="true" />
              </Button>
            </div>
          </div>
        </header>

        <main id="main-content" tabIndex={-1} className="relative z-10">
          {/* Asymmetric Hero */}
          <section className="relative overflow-hidden px-4 pt-10 pb-16 sm:px-6 sm:pt-16 sm:pb-24">
            <div className="relative z-10 mx-auto grid max-w-6xl grid-cols-1 items-center gap-10 lg:grid-cols-[minmax(0,0.92fr)_minmax(0,1.08fr)] lg:gap-10">
              {/* Left Column */}
              <div className="flex min-w-0 flex-col items-start gap-5 text-left">
                <BlurFade delay={0.04}>
                  <a
                    href="#ecosystem"
                    className="inline-flex items-center gap-2 rounded-full border border-border bg-card/80 px-3 py-1 text-xs whitespace-nowrap text-muted-foreground backdrop-blur-md transition-colors hover:border-primary/35 hover:text-foreground"
                  >
                    <span className="rounded-full bg-primary px-2 py-0.5 font-mono text-[10px] font-semibold text-primary-foreground">
                      NEW
                    </span>
                    <span>Rust CLI · Karma Tech Project</span>
                    <ArrowRight className="size-3.5" aria-hidden="true" />
                  </a>
                </BlurFade>

                <div className="flex items-center gap-3">
                  <img
                    src="/karma-mark.png"
                    alt="Karma Tech Project"
                    width={358}
                    height={199}
                    fetchPriority="high"
                    className="h-8 w-auto sm:h-9"
                  />
                  <span className="text-base font-medium tracking-tight text-foreground sm:text-lg">
                    Mehhcode
                  </span>
                </div>

                <h1 className="text-3xl font-medium tracking-[-0.035em] text-foreground sm:text-4xl lg:text-[2.85rem] lg:leading-[1.08]">
                  <span className="sr-only">
                    Rust coding agent, with native metasearch, QuickJS codemode,
                    and 5-layer Rust.
                  </span>
                  <span aria-hidden="true">
                    Rust coding agent,{" "}
                    <span className="block min-h-[2.3em] text-primary sm:min-h-[1.18em]">
                      <TypingAnimation
                        words={[
                          "with native metasearch.",
                          "with QuickJS codemode.",
                          "built on 5-layer Rust.",
                        ]}
                        loop
                        paused={motionPaused}
                        typeSpeed={55}
                        deleteSpeed={32}
                        pauseDelay={2200}
                        className="leading-[1.12] tracking-[-0.035em]"
                      />
                    </span>
                  </span>
                </h1>

                <p className="max-w-lg text-base leading-relaxed text-muted-foreground">
                  Native Rust coding agent with keyless{" "}
                  <code className="font-mono text-sm text-foreground">
                    MehhNative
                  </code>{" "}
                  metasearch, QuickJS tool batching, BM25 deferred MCP, and
                  Lead/Worker routing.
                </p>

                <div className="flex flex-wrap items-center gap-3 pt-1">
                  <ShimmerButton
                    href="#install"
                    background="var(--foreground)"
                    shimmerColor="#fda4af"
                    className="h-11 px-6 text-sm font-medium text-background"
                  >
                    <Terminal className="mr-2 size-4" aria-hidden="true" />
                    <span>Install mehh</span>
                  </ShimmerButton>

                  <Button
                    variant="secondary"
                    size="lg"
                    nativeButton={false}
                    className="h-11 rounded-full border border-border/80 bg-secondary/90 px-6 text-sm font-medium text-secondary-foreground hover:bg-accent"
                    render={
                      <a
                        href={MEHH_REPO_URL}
                        target="_blank"
                        rel="noopener noreferrer"
                      />
                    }
                  >
                    <span>View on GitHub</span>
                    <ArrowUpRight data-icon="inline-end" aria-hidden="true" />
                  </Button>
                </div>
              </div>

              {/* Right Column: Authentic Interactive mehh TUI Stage */}
              <div className="w-full min-w-0">
                <ProductStage />
              </div>
            </div>
          </section>

          {/* Capabilities Marquee Strip with Pause Control */}
          <div className="relative border-y border-border/75 bg-card/50 py-3 backdrop-blur-sm">
            <div className="mx-auto flex max-w-6xl items-center gap-3 px-4 sm:px-6">
              <Marquee
                pauseOnHover
                paused={motionPaused}
                className="min-w-0 flex-1 [--duration:34s] [--gap:1.25rem]"
              >
                {MARQUEE_CAPABILITIES.map((item) => (
                  <span
                    key={item}
                    className="inline-flex items-center gap-2 rounded-full border border-border/80 bg-background/85 px-3.5 py-1 font-mono text-xs text-muted-foreground"
                  >
                    <span
                      className="size-1.5 rounded-full bg-primary/80"
                      aria-hidden="true"
                    />
                    <span>{item}</span>
                  </span>
                ))}
              </Marquee>

              <button
                type="button"
                onClick={() => setMotionPaused((prev) => !prev)}
                aria-label={
                  motionPaused ? "Play animations" : "Pause animations"
                }
                className="inline-flex min-h-7 min-w-7 shrink-0 cursor-pointer items-center justify-center rounded-full border border-border/80 bg-background/85 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
              >
                {motionPaused ? (
                  <Play className="size-3.5" aria-hidden="true" />
                ) : (
                  <Pause className="size-3.5" aria-hidden="true" />
                )}
              </button>
            </div>
          </div>

          {/* 3 Core Strengths */}
          <EditorialFeatures />

          <Separator className="mx-auto max-w-6xl opacity-70" />

          {/* Karma Tech Project Ecosystem */}
          <EcosystemSection />

          {/* Asymmetric Quickstart Split */}
          <section
            id="install"
            className="mx-auto w-full max-w-6xl px-4 pt-8 pb-24 sm:px-6 sm:pb-28"
          >
            <div className="grid grid-cols-1 gap-10 rounded-2xl border border-border bg-card/75 p-6 backdrop-blur-xl sm:p-10 lg:grid-cols-[0.85fr_1.15fr] lg:items-start">
              <div className="flex flex-col gap-3 text-left">
                <span className="font-mono text-xs font-medium tracking-wider text-primary uppercase">
                  Quickstart
                </span>
                <h2 className="text-2xl font-semibold tracking-tight text-foreground sm:text-3xl">
                  Up and running in three commands.
                </h2>
                <p className="text-sm leading-relaxed text-muted-foreground">
                  Requires Rust (Edition 2021) and a C compiler (MSVC, Clang, or
                  gcc) for QuickJS.
                </p>
              </div>

              <div className="flex min-w-0 flex-col divide-y divide-border/80 border-y border-border/80">
                {QUICKSTART_STEPS.map((item) => (
                  <div
                    key={item.step}
                    className="flex flex-col justify-between gap-2.5 py-4 text-left"
                  >
                    <div className="flex items-baseline gap-3">
                      <span className="font-mono text-xs font-semibold text-primary">
                        {item.step}
                      </span>
                      <span className="text-sm font-medium text-foreground">
                        {item.title}
                      </span>
                    </div>

                    <div className="flex items-center justify-between gap-3 rounded-lg border border-border bg-background/95 px-3.5 py-2">
                      <code className="truncate font-mono text-xs text-foreground">
                        {item.command}
                      </code>
                      <CopyButton
                        text={item.command}
                        label={item.copyLabel}
                        className="min-h-7 min-w-7 border-0 bg-transparent px-1"
                      />
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </section>
        </main>

        {/* Footer */}
        <footer className="relative z-10 border-t border-border/80 bg-background/70 backdrop-blur-md">
          <div className="mx-auto flex max-w-6xl flex-col gap-12 px-4 pt-14 pb-8 sm:px-6">
            <div className="grid grid-cols-1 gap-10 text-left sm:grid-cols-2 lg:grid-cols-[1.5fr_1fr_1fr]">
              <div className="flex flex-col items-start gap-4">
                <img
                  src="/karma-logo-light.png"
                  alt="Karma Tech Project"
                  width={434}
                  height={336}
                  loading="lazy"
                  decoding="async"
                  className="h-10 w-auto dark:hidden"
                />
                <img
                  src="/karma-logo-dark.png"
                  alt="Karma Tech Project"
                  width={434}
                  height={336}
                  loading="lazy"
                  decoding="async"
                  className="hidden h-10 w-auto dark:block"
                />
                <p className="max-w-xs text-sm text-muted-foreground">
                  Open-source developer tools built for speed and verifiable
                  architecture.
                </p>
              </div>

              <div className="flex flex-col gap-3 text-sm">
                <div className="font-medium text-foreground">Mehhcode</div>
                <nav
                  aria-label="Mehhcode sections"
                  className="flex flex-col gap-2 text-muted-foreground"
                >
                  <a href="#capabilities" className="hover:text-foreground">
                    Highlights
                  </a>
                  <a href="#architecture" className="hover:text-foreground">
                    5-Layer Architecture
                  </a>
                  <a href="#install" className="hover:text-foreground">
                    Quickstart
                  </a>
                  <a
                    href={MEHH_REPO_URL}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="hover:text-foreground"
                  >
                    GitHub Repository
                  </a>
                </nav>
              </div>

              <div className="flex flex-col gap-3 text-sm">
                <div className="font-medium text-foreground">
                  Karma Ecosystem
                </div>
                <nav
                  aria-label="Ecosystem projects"
                  className="flex flex-col gap-2 text-muted-foreground"
                >
                  {COMPANION_PROJECTS.map((project) => (
                    <a
                      key={project.id}
                      href={project.repoUrl}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="hover:text-foreground"
                    >
                      {project.name}
                    </a>
                  ))}
                  <a
                    href="https://github.com/karmavn"
                    target="_blank"
                    rel="noopener noreferrer"
                    className="hover:text-foreground"
                  >
                    github.com/karmavn
                  </a>
                </nav>
              </div>
            </div>

            <div
              aria-hidden="true"
              className="select-none overflow-hidden py-2 text-left"
            >
              <div className="text-[clamp(3.2rem,14vw,10.5rem)] leading-none font-semibold tracking-[-0.045em] text-foreground/92">
                Mehhcod
                <span className="inline-block -translate-y-2 rotate-6 text-primary sm:-translate-y-4">
                  e
                </span>
              </div>
            </div>

            <div className="flex flex-wrap items-center justify-between gap-4 border-t border-border/80 pt-6 text-xs text-muted-foreground">
              <span>Karma Tech Project · MIT License</span>
              <span>Rust · Tauri · Electron · Android</span>
            </div>
          </div>
        </footer>
      </div>
    </MotionConfig>
  )
}

export default App
