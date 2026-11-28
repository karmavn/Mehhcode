import { useState } from "react"
import { Tabs as TabsPrimitive } from "@base-ui/react/tabs"
import { Layers, Search, Terminal, Zap } from "lucide-react"

import { cn } from "@/lib/utils"
import {
  ARCHITECTURE_LAYERS,
  CONTEXT_ENGINE_TABS,
  SEARCH_PIPELINE_MODES,
} from "@/data/projects"
import { CopyButton } from "@/components/copy-button"
import { Badge } from "@/components/ui/badge"
import { BlurFade } from "@/components/ui/blur-fade"
import { MagicCard } from "@/components/ui/magic-card"
import { Separator } from "@/components/ui/separator"
import { TextAnimate } from "@/components/ui/text-animate"

export function EditorialFeatures() {
  const [selectedSearchId, setSelectedSearchId] = useState(
    SEARCH_PIPELINE_MODES[0].id
  )
  const [selectedContextId, setSelectedContextId] = useState(
    CONTEXT_ENGINE_TABS[0].id
  )

  return (
    <div
      id="capabilities"
      className="mx-auto flex w-full max-w-6xl flex-col gap-20 px-4 py-16 sm:px-6 sm:py-22"
    >
      {/* Editorial Statement */}
      <section className="grid grid-cols-1 items-end gap-8 border-b border-border/80 pb-12 lg:grid-cols-[minmax(0,1.4fr)_minmax(0,0.6fr)]">
        <div className="min-w-0 text-left">
          <TextAnimate
            as="h2"
            by="word"
            animation="blurInUp"
            once
            className="text-3xl font-medium tracking-[-0.03em] text-foreground sm:text-5xl sm:leading-[1.1]"
          >
            Three core advantages: native metasearch, zero-waste context, and 5-layer Rust.
          </TextAnimate>
        </div>

        <div className="flex min-w-0 flex-col items-start gap-2.5 text-left lg:items-end lg:text-right">
          <span className="font-mono text-xs text-primary">
            10 Rust Crates · 30 Slash Commands · MIT License
          </span>
          <p className="text-sm leading-relaxed text-muted-foreground">
            Keyless metasearch, QuickJS batching, BM25 MCP search, and JSONL v3
            sessions.
          </p>
        </div>
      </section>

      {/* Highlight 01: Mehh Search */}
      <section className="grid grid-cols-1 items-center gap-10 lg:grid-cols-[minmax(0,0.9fr)_minmax(0,1.1fr)] lg:gap-14">
        <BlurFade
          inView
          className="flex min-w-0 flex-col items-start gap-3.5 text-left"
        >
          <span className="font-mono text-xs font-medium tracking-wider text-primary uppercase">
            01 · Mehh Search (MehhNative + WebRouter)
          </span>
          <h3 className="text-2xl font-semibold tracking-tight text-foreground sm:text-4xl">
            Keyless in-process metasearch with Stage-2 Auto-Enrich.
          </h3>
          <p className="text-base leading-relaxed text-muted-foreground">
            <code className="font-mono text-sm text-foreground">
              classify_query
            </code>{" "}
            dispatches DuckDuckGo, Bing, and intent JSON APIs (crates.io, npm,
            GitHub, StackOverflow) via{" "}
            <code className="font-mono text-sm text-foreground">RRF k=60</code>,
            enriches top 3 hits into &le; 900-char Markdown, and fails over
            across 7 keyless providers.
          </p>
        </BlurFade>

        <BlurFade inView delay={0.08} className="min-w-0">
          <MagicCard
            className="rounded-2xl"
            gradientSize={260}
            gradientOpacity={0.48}
            gradientFrom="#e11d48"
            gradientTo="#d97757"
            gradientColor="rgba(225, 29, 72, 0.08)"
          >
            <TabsPrimitive.Root
              value={selectedSearchId}
              onValueChange={(val) => setSelectedSearchId(String(val))}
              className="flex flex-col gap-4 p-5 sm:p-6"
            >
              <TabsPrimitive.List
                aria-label="Inspect Mehh Search pipeline stages"
                className="flex gap-1.5 overflow-x-auto pb-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
              >
                {SEARCH_PIPELINE_MODES.map((mode) => (
                  <TabsPrimitive.Tab
                    key={mode.id}
                    value={mode.id}
                    className={cn(
                      "relative shrink-0 cursor-pointer rounded-full border border-border bg-background/85 px-3 py-1.5 text-xs font-medium text-muted-foreground transition-colors hover:text-foreground",
                      "aria-selected:border-primary/40 aria-selected:bg-foreground aria-selected:text-background",
                      "aria-selected:after:absolute aria-selected:after:inset-x-3 aria-selected:after:bottom-0.5 aria-selected:after:border-b-2 aria-selected:after:border-primary"
                    )}
                  >
                    {mode.label}
                  </TabsPrimitive.Tab>
                ))}
              </TabsPrimitive.List>

              {SEARCH_PIPELINE_MODES.map((mode) => (
                <TabsPrimitive.Panel
                  key={mode.id}
                  value={mode.id}
                  tabIndex={0}
                  className="flex min-w-0 flex-col gap-3 rounded-xl border border-border/80 bg-background/95 p-4 text-left"
                >
                  <div className="flex items-center justify-between gap-2">
                    <div className="flex min-w-0 items-center gap-2 font-mono text-xs text-primary">
                      <Search
                        className="size-3.5 shrink-0"
                        aria-hidden="true"
                      />
                      <span className="truncate">{mode.queryExample}</span>
                    </div>
                    <Badge
                      variant="secondary"
                      className="shrink-0 font-mono text-[10px]"
                    >
                      Keyless
                    </Badge>
                  </div>

                  <Separator />

                  <div className="flex flex-col gap-2 text-xs">
                    <div>
                      <span className="font-mono font-semibold text-foreground">
                        Stage 1 Sources:{" "}
                      </span>
                      <span className="text-muted-foreground">
                        {mode.stage1Sources}
                      </span>
                    </div>
                    <div>
                      <span className="font-mono font-semibold text-foreground">
                        Ranking &amp; Budget:{" "}
                      </span>
                      <span className="text-muted-foreground">
                        {mode.stage1Budget}
                      </span>
                    </div>
                    <div>
                      <span className="font-mono font-semibold text-foreground">
                        Stage 2 Auto-Enrich:{" "}
                      </span>
                      <span className="text-muted-foreground">
                        {mode.stage2Enrich}
                      </span>
                    </div>
                  </div>

                  <div className="rounded-lg border border-border/70 bg-muted/40 px-3 py-2 font-mono text-[11px] text-foreground/90">
                    {mode.routerChain}
                  </div>
                </TabsPrimitive.Panel>
              ))}
            </TabsPrimitive.Root>
          </MagicCard>
        </BlurFade>
      </section>

      {/* Highlight 02: Context-Tight Execution */}
      <section className="grid grid-cols-1 items-center gap-10 lg:grid-cols-[minmax(0,1.1fr)_minmax(0,0.9fr)] lg:gap-14">
        <BlurFade inView delay={0.08} className="order-2 min-w-0 lg:order-1">
          <MagicCard
            className="rounded-2xl"
            gradientSize={260}
            gradientOpacity={0.48}
            gradientFrom="#e11d48"
            gradientTo="#d97757"
            gradientColor="rgba(225, 29, 72, 0.08)"
          >
            <TabsPrimitive.Root
              value={selectedContextId}
              onValueChange={(val) => setSelectedContextId(String(val))}
              className="flex flex-col gap-4 p-5 sm:p-6"
            >
              <TabsPrimitive.List
                aria-label="Inspect Mehhcode context optimizations"
                className="flex gap-1.5 overflow-x-auto pb-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
              >
                {CONTEXT_ENGINE_TABS.map((tab) => (
                  <TabsPrimitive.Tab
                    key={tab.id}
                    value={tab.id}
                    className={cn(
                      "relative shrink-0 cursor-pointer rounded-full border border-border bg-background/85 px-3 py-1.5 text-xs font-medium text-muted-foreground transition-colors hover:text-foreground",
                      "aria-selected:border-primary/40 aria-selected:bg-foreground aria-selected:text-background",
                      "aria-selected:after:absolute aria-selected:after:inset-x-3 aria-selected:after:bottom-0.5 aria-selected:after:border-b-2 aria-selected:after:border-primary"
                    )}
                  >
                    {tab.label}
                  </TabsPrimitive.Tab>
                ))}
              </TabsPrimitive.List>

              {CONTEXT_ENGINE_TABS.map((tab) => (
                <TabsPrimitive.Panel
                  key={tab.id}
                  value={tab.id}
                  tabIndex={-1}
                  className="flex min-w-0 flex-col gap-3 rounded-xl border border-border/80 bg-background/95 p-4 text-left outline-none"
                >
                  <div className="flex items-center justify-between gap-2">
                    <div className="flex min-w-0 items-center gap-2 font-mono text-[11px] text-muted-foreground">
                      <Zap
                        className="size-3.5 shrink-0 text-primary"
                        aria-hidden="true"
                      />
                      <span className="truncate">{tab.sourceFile}</span>
                    </div>
                    <CopyButton text={tab.copyText} label={tab.copyLabel} />
                  </div>

                  <p className="text-xs leading-relaxed text-muted-foreground">
                    {tab.summary}
                  </p>

                  <pre className="overflow-x-auto rounded-lg border border-border/80 bg-muted/35 p-3 font-mono text-[11px] leading-5 text-foreground sm:text-xs">
                    <code>{tab.codeLines.join("\n")}</code>
                  </pre>

                  <div className="font-mono text-[11px] text-primary">
                    {tab.footnote}
                  </div>
                </TabsPrimitive.Panel>
              ))}
            </TabsPrimitive.Root>
          </MagicCard>
        </BlurFade>

        <BlurFade
          inView
          className="order-1 flex min-w-0 flex-col items-start gap-3.5 text-left lg:order-2"
        >
          <span className="font-mono text-xs font-medium tracking-wider text-primary uppercase">
            02 · Zero-Waste Context Engine
          </span>
          <h3 className="text-2xl font-semibold tracking-tight text-foreground sm:text-4xl">
            Collapse multi-turn tool loops and MCP schema bloat.
          </h3>
          <p className="text-base leading-relaxed text-muted-foreground">
            With{" "}
            <code className="font-mono text-sm text-foreground">
              --codemode
            </code>
            , embedded{" "}
            <code className="font-mono text-sm text-foreground">rquickjs</code>{" "}
            executes{" "}
            <code className="font-mono text-xs text-foreground">
              await tools.&lt;name&gt;(args)
            </code>{" "}
            in one turn, while BM25 indexing defers large MCP schemas,{" "}
            <code className="font-mono text-xs text-foreground">analyze</code>{" "}
            scans brace-aware symbols, and{" "}
            <code className="font-mono text-xs text-foreground">
              --worker-model
            </code>{" "}
            adds 2-turn Lead rescue.
          </p>
        </BlurFade>
      </section>

      {/* Highlight 03: 5-Layer Rust Architecture */}
      <section
        id="architecture"
        className="grid grid-cols-1 items-start gap-10 pt-2 lg:grid-cols-[minmax(0,0.85fr)_minmax(0,1.15fr)] lg:gap-14"
      >
        <BlurFade
          inView
          className="flex min-w-0 flex-col items-start gap-3.5 text-left"
        >
          <span className="font-mono text-xs font-medium tracking-wider text-primary uppercase">
            03 · 5-Layer Rust Architecture
          </span>
          <h3 className="text-2xl font-semibold tracking-tight text-foreground sm:text-4xl">
            Ten crates, strict DAG boundaries, and non-blocking TUI.
          </h3>
          <p className="text-base leading-relaxed text-muted-foreground">
            Every dependency edge is verified by{" "}
            <code className="font-mono text-sm text-foreground">
              workspace_guardrails
            </code>
            , with CI forbidding{" "}
            <code className="font-mono text-xs text-foreground">
              unwrap_used
            </code>{" "}
            and <code className="font-mono text-xs text-foreground">panic</code>{" "}
            in production code.
          </p>
          <div className="flex flex-col gap-2 pt-1 text-sm text-muted-foreground">
            <div className="flex items-center gap-2">
              <Terminal
                className="size-4 shrink-0 text-primary"
                aria-hidden="true"
              />
              <span>
                Non-blocking{" "}
                <code className="font-mono text-xs">try_lock()</code> TUI +
                PasteBurst + 30 slash commands
              </span>
            </div>
            <div className="flex items-center gap-2">
              <Layers
                className="size-4 shrink-0 text-primary"
                aria-hidden="true"
              />
              <span>
                JSONL v3 sessions (
                <code className="font-mono text-xs">/tree</code>,{" "}
                <code className="font-mono text-xs">/fork</code>,{" "}
                <code className="font-mono text-xs">/compact</code>) &amp;{" "}
                <code className="font-mono text-xs">undo_edit</code>
              </span>
            </div>
          </div>
        </BlurFade>

        <BlurFade inView delay={0.08} className="min-w-0">
          <MagicCard
            className="rounded-2xl"
            gradientSize={260}
            gradientOpacity={0.48}
            gradientFrom="#e11d48"
            gradientTo="#d97757"
            gradientColor="rgba(225, 29, 72, 0.08)"
          >
            <div className="flex flex-col divide-y divide-border/80 p-5 sm:p-6">
              {ARCHITECTURE_LAYERS.map((group) => (
                <div
                  key={group.layer}
                  className="grid grid-cols-1 gap-2 rounded-lg px-2.5 py-3.5 transition-colors first:pt-2 last:pb-2 hover:bg-accent/45 sm:grid-cols-[120px_minmax(0,1fr)] sm:gap-4"
                >
                  <div className="flex items-baseline gap-2 text-left">
                    <span className="font-mono text-xs font-semibold text-primary">
                      {group.layer}
                    </span>
                    <span className="text-xs font-medium text-foreground">
                      {group.name}
                    </span>
                  </div>

                  <div className="flex flex-col gap-1.5 text-left">
                    {group.crates.map((crate) => (
                      <div
                        key={crate.name}
                        className="flex flex-col gap-0.5 sm:flex-row sm:items-baseline sm:justify-between sm:gap-4"
                      >
                        <code className="shrink-0 font-mono text-xs font-semibold text-foreground">
                          {crate.name}
                        </code>
                        <span className="text-xs text-muted-foreground sm:text-right">
                          {crate.role}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          </MagicCard>
        </BlurFade>
      </section>
    </div>
  )
}
