import { useState } from "react"
import { Tabs as TabsPrimitive } from "@base-ui/react/tabs"

import { cn } from "@/lib/utils"
import { TERMINAL_MODES } from "@/data/projects"
import { CopyButton } from "@/components/copy-button"
import { BorderBeam } from "@/components/ui/border-beam"

export function ProductStage() {
  const [selectedId, setSelectedId] = useState(TERMINAL_MODES[0].id)
  const [thoughtOpen, setThoughtOpen] = useState(false)

  const handleSelectScenario = (nextValue: unknown) => {
    setSelectedId(String(nextValue))
    setThoughtOpen(false)
  }

  return (
    <div
      style={{
        colorScheme: "dark",
        scrollbarColor:
          "var(--color-scrollbar-thumb) var(--color-scrollbar-track)",
      }}
      className="relative w-full min-w-0 overflow-hidden rounded-2xl border border-[#2A2D35] bg-[#0b0c10] text-[#E4E4E7] shadow-[0_24px_70px_-20px_rgba(15,17,24,0.45)]"
    >
      <BorderBeam
        size={160}
        duration={10}
        colorFrom="#d97757"
        colorTo="#e11d48"
        borderWidth={1.25}
      />

      <TabsPrimitive.Root
        value={selectedId}
        onValueChange={handleSelectScenario}
      >
        {/* Windows Terminal Title & Single-Row Scenario Tab Bar */}
        <div className="flex items-center justify-between gap-2 border-b border-[#262932] bg-[#15171e] px-3 py-2">
          <div className="flex min-w-0 flex-1 items-center gap-1.5">
            <div className="hidden shrink-0 items-center gap-1.5 rounded-md border border-[#2A2D35] bg-[#0b0c10] px-2.5 py-1 font-mono text-[11px] text-[#E4E4E7] sm:flex">
              <span className="text-[#8E8E98]">&gt;_</span>
              <span>cmd.exe</span>
            </div>

            <TabsPrimitive.List
              aria-label="Mehhcode TUI scenarios"
              className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
            >
              {TERMINAL_MODES.map((item) => (
                <TabsPrimitive.Tab
                  key={item.id}
                  value={item.id}
                  className={cn(
                    "relative min-h-6 shrink-0 cursor-pointer rounded-md border border-transparent px-2 py-0.5 font-mono text-[11px] whitespace-nowrap text-[#8E8E98] transition-colors hover:bg-[#1c1f27] hover:text-[#E4E4E7]",
                    "aria-selected:border-[#D97757]/60 aria-selected:bg-[#23262E] aria-selected:text-[#E4E4E7]",
                    "aria-selected:after:absolute aria-selected:after:inset-x-1.5 aria-selected:after:bottom-0 aria-selected:after:border-b-2 aria-selected:after:border-[#D97757]"
                  )}
                >
                  {item.tabLabel}
                </TabsPrimitive.Tab>
              ))}
            </TabsPrimitive.List>
          </div>

          <div
            aria-hidden="true"
            className="hidden shrink-0 items-center gap-3 pr-1 font-mono text-xs text-[#8E8E98] sm:flex"
          >
            <span>—</span>
            <span>□</span>
            <span>×</span>
          </div>
        </div>

        {/* Scenario Panel: Header + Transcript + Prompt + Status */}
        {TERMINAL_MODES.map((scenario) => {
          const filledBlocks = Math.min(
            8,
            Math.max(1, Math.ceil(scenario.ctxPct / 13))
          )

          return (
            <TabsPrimitive.Panel
              key={scenario.id}
              value={scenario.id}
              tabIndex={-1}
              className="outline-none"
            >
              {/* Authentic mehh Session Header Row */}
              <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5 border-b border-[#2A2D35] px-3.5 pt-2.5 pb-2 font-mono text-[11px] leading-relaxed">
                <div className="flex flex-wrap items-baseline gap-x-1.5 gap-y-0.5">
                  <span className="font-bold text-[#D97757]">mehh</span>
                  <span className="font-semibold text-[#E4E4E7]">
                    {scenario.modelLabel}
                  </span>
                  <span className="text-[#8E8E98]">
                    ({scenario.providerSlug})
                  </span>
                  <span className="text-[#D97757]">
                    {scenario.modeTag} [+ connect]
                  </span>
                </div>
                <div className="flex flex-wrap items-baseline gap-x-1.5 gap-y-0.5">
                  <span className="text-[#FBBF24]">
                    session: {scenario.sessionId} ▾
                  </span>
                  <span className="text-[#D97757]">[+ new]</span>
                  <span className="text-[#8E8E98]">· {scenario.cwd}</span>
                </div>
              </div>

              {/* Transcript Body */}
              <div className="flex min-h-[235px] flex-col justify-between gap-4 px-3.5 py-2.5 font-mono text-xs sm:min-h-[250px]">
                <div className="flex flex-col">
                  {/* User Message Band */}
                  <div className="w-full border-l-[3px] border-[#D97757] bg-[#23262E] px-2.5 py-1 text-left text-[#E4E4E7]">
                    {scenario.userPrompt}
                  </div>

                  {/* Clickable Reasoning Expander */}
                  <button
                    type="button"
                    aria-expanded={thoughtOpen}
                    onClick={() => setThoughtOpen((prev) => !prev)}
                    className="mt-2.5 flex min-h-6 w-fit cursor-pointer items-center gap-1.5 text-left font-mono text-xs italic text-[#8E8E98] transition-colors hover:text-[#E4E4E7]"
                  >
                    <span>{thoughtOpen ? "▾" : "▸"}</span>
                    <span>{scenario.thoughtSummary}</span>
                  </button>

                  {thoughtOpen && (
                    <div className="mt-1 flex flex-col gap-0.5 border-l border-[#2A2D35] pl-3 text-left text-[11px] italic text-[#A1A1AA]">
                      {scenario.thoughtExpandedLines.map((line) => (
                        <div key={line}>{line}</div>
                      ))}
                    </div>
                  )}

                  {/* Tool Calls & Diffs */}
                  {scenario.toolCalls && scenario.toolCalls.length > 0 && (
                    <div className="mt-2.5 flex flex-col gap-2 text-left text-[11px]">
                      {scenario.toolCalls.map((call) => (
                        <div
                          key={`${call.toolName}-${call.argsSummary}`}
                          className="flex flex-col gap-1"
                        >
                          <div className="flex flex-wrap items-baseline gap-x-1.5">
                            <span className="text-[#4ADE80]">✓</span>
                            <span className="font-medium text-[#E4E4E7]">
                              {call.toolName}
                            </span>
                            <span className="text-[#D97757]">
                              {call.argsSummary}
                            </span>
                            {call.meta && (
                              <span className="text-[#8E8E98]">
                                · {call.meta}
                              </span>
                            )}
                          </div>
                          {call.detail && (
                            <div className="pl-3 text-[#8E8E98]">
                              {call.detail}
                            </div>
                          )}
                          {call.diffLines && call.diffLines.length > 0 && (
                            <div className="overflow-x-auto rounded border border-[#2A2D35] bg-[#12141a] py-1">
                              {call.diffLines.map((line) => (
                                <div
                                  key={line.text}
                                  className={cn(
                                    "px-2.5 whitespace-pre",
                                    line.kind === "add" &&
                                      "bg-[#162B20] text-[#4ADE80]",
                                    line.kind === "del" &&
                                      "bg-[#2D181C] text-[#F87171]",
                                    line.kind === "ctx" && "text-[#8E8E98]"
                                  )}
                                >
                                  {line.text}
                                </div>
                              ))}
                            </div>
                          )}
                        </div>
                      ))}
                    </div>
                  )}

                  {/* Assistant Response */}
                  <div className="mt-1.5 flex flex-col gap-1 text-left leading-relaxed text-[#E4E4E7]">
                    {scenario.assistantLines.map((line) => (
                      <p key={line}>{line}</p>
                    ))}
                  </div>
                </div>
              </div>

              {/* Authentic Prompt Box & Status Telemetry Footer */}
              <div className="px-3.5 pt-2 pb-2.5 font-mono text-xs">
                <div className="relative rounded-[3px] border border-[#3A3D45] px-2.5 py-1.5">
                  <span className="absolute -top-2 left-2 bg-[#0b0c10] px-1 text-[11px] leading-none text-[#E4E4E7]">
                    Prompt
                  </span>
                  <div className="flex items-center justify-between gap-2">
                    <span className="flex min-w-0 items-center truncate text-[#8E8E98]">
                      <span className="mr-1.5 text-[#8E8E98]">&gt;</span>
                      <span
                        aria-hidden="true"
                        className="mr-1 inline-block h-3.5 w-2 bg-[#8E8E98]/85 align-baseline motion-safe:animate-pulse"
                      />
                      <span className="truncate">
                        Type a message or /help...
                      </span>
                    </span>
                    <CopyButton
                      text={scenario.copyCommand}
                      label={`Copy ${scenario.tabLabel} command`}
                      className="min-h-6 shrink-0 border-[#2A2D35] bg-[#16181d] px-2 py-0.5 text-[11px] text-[#A1A1AA] hover:bg-[#23262E] hover:text-[#E4E4E7]"
                    />
                  </div>
                </div>

                <div className="mt-2 flex flex-wrap items-center justify-between gap-2 text-[11px] text-[#8E8E98]">
                  <div className="flex items-center gap-1.5">
                    <span>
                      thinking:{" "}
                      <span
                        className={
                          scenario.thinkingStatus === "n/a"
                            ? "text-[#8E8E98]"
                            : "text-[#D97757]"
                        }
                      >
                        {scenario.thinkingStatus}
                      </span>{" "}
                      · ctx {scenario.ctxPct}%
                    </span>
                    <span
                      className="inline-flex items-center gap-[1px]"
                      aria-hidden="true"
                    >
                      {Array.from({ length: 8 }).map((_, i) => (
                        <span
                          key={i}
                          className={
                            i < filledBlocks
                              ? "text-[#D97757]"
                              : "text-[#2A2D35]"
                          }
                        >
                          {i < filledBlocks ? "█" : "░"}
                        </span>
                      ))}
                    </span>
                  </div>
                  <span>
                    {scenario.usageSummary} · {scenario.costSummary} ·{" "}
                    <span className="text-[#D97757]">/help</span>
                  </span>
                </div>
              </div>
            </TabsPrimitive.Panel>
          )
        })}
      </TabsPrimitive.Root>
    </div>
  )
}
