export interface TuiToolCall {
  toolName: string
  argsSummary: string
  meta?: string
  detail?: string
  diffLines?: ReadonlyArray<{
    kind: "ctx" | "del" | "add"
    text: string
  }>
}

export interface TuiScenario {
  id: string
  tabLabel: string
  modelLabel: string
  providerSlug: string
  modeTag: string
  sessionId: string
  cwd: string
  userPrompt: string
  thoughtSummary: string
  thoughtExpandedLines: ReadonlyArray<string>
  toolCalls?: ReadonlyArray<TuiToolCall>
  assistantLines: ReadonlyArray<string>
  copyCommand: string
  thinkingStatus: string
  ctxPct: number
  usageSummary: string
  costSummary: string
}

export interface SearchPipelineMode {
  id: string
  label: string
  queryExample: string
  stage1Sources: string
  stage1Budget: string
  stage2Enrich: string
  routerChain: string
}

export interface ContextEngineTab {
  id: string
  label: string
  sourceFile: string
  summary: string
  codeLines: ReadonlyArray<string>
  copyText: string
  copyLabel: string
  footnote: string
}

export interface ArchitectureLayer {
  layer: string
  name: string
  crates: ReadonlyArray<{
    name: string
    role: string
  }>
}

export interface CompanionProject {
  id: "kbrowser" | "optimum-geogebra" | "karmalaucher"
  index: string
  name: string
  tagline: string
  summary: string
  repoUrl: string
  stack: ReadonlyArray<string>
  highlights: ReadonlyArray<string>
  accentLabel: string
}

export const MEHH_REPO_URL = "https://github.com/karmavn/Mehhcode"

export const MEHH_INSTALL_COMMAND =
  "git clone https://github.com/karmavn/Mehhcode.git && cd Mehhcode && cargo install --path crates/mehh-cli"

export const MARQUEE_CAPABILITIES: ReadonlyArray<string> = [
  "MehhNative Metasearch",
  "RRF k=60 Fusion",
  "Stage-2 Auto-Enrich",
  "7 Keyless Providers",
  "QuickJS Codemode",
  "BM25 Deferred MCP",
  "Brace-Aware Analyzer",
  "Lead / Worker Routing",
  "5-Layer Rust DAG",
  "JSONL v3 /tree & /fork",
  "30 Slash Commands",
  "Headless JSON-RPC",
]

export const TERMINAL_MODES: ReadonlyArray<TuiScenario> = [
  {
    id: "live",
    tabLabel: "Live TUI",
    modelLabel: "Nemotron 3.5 Super VL (OpenRouter, free)",
    providerSlug: "openrouter",
    modeTag: "[smart]",
    sessionId: "01a22159",
    cwd: "D:/Mehhcode",
    userPrompt: "hi",
    thoughtSummary: "Thought for 1.7s (1 lines · Ctrl+T to expand)",
    thoughtExpandedLines: [
      "Workspace D:/Mehhcode ready; awaiting prompt or slash command.",
    ],
    assistantLines: ["Hi. How can I help you with the mehh workspace today?"],
    copyCommand: "mehh",
    thinkingStatus: "n/a",
    ctxPct: 1,
    usageSummary: "7.2k in / 64 out",
    costSummary: "$0.0000",
  },
  {
    id: "search",
    tabLabel: "Mehh Search",
    modelLabel: "Claude Sonnet 4.5",
    providerSlug: "anthropic",
    modeTag: "[smart]",
    sessionId: "4c9e10b2",
    cwd: "D:/Mehhcode",
    userPrompt: "check tokio JoinSet::abort_all cancellation behavior",
    thoughtSummary: "Thought for 1.2s (2 lines · Ctrl+T to expand)",
    thoughtExpandedLines: [
      "Intent: rust_crates -> parallel DuckDuckGo, Bing, crates.io.",
      "Fusing via RRF (k=60) + Stage-2 Markdown passage extraction.",
    ],
    toolCalls: [
      {
        toolName: "web_search",
        argsSummary: '"tokio JoinSet abort_all"',
        meta: "MehhNative · 410ms",
        detail: "RRF k=60 -> enriched top 3 docs.rs passages (keyless, cached)",
      },
    ],
    assistantLines: [
      "abort_all() schedules cancellation on all tasks; await join_next() until None to drain.",
    ],
    copyCommand: 'mehh -p "check tokio JoinSet::abort_all cancellation behavior"',
    thinkingStatus: "medium",
    ctxPct: 8,
    usageSummary: "9.4k in / 192 out",
    costSummary: "$0.0280",
  },
  {
    id: "codemode",
    tabLabel: "Codemode",
    modelLabel: "Gemini 2.5 Pro",
    providerSlug: "google",
    modeTag: "[smart]",
    sessionId: "7f3a88d1",
    cwd: "D:/Mehhcode",
    userPrompt: "scan mehh-agent symbols and abort tasks on cancel",
    thoughtSummary: "Thought for 2.1s (2 lines · Ctrl+T to expand)",
    thoughtExpandedLines: [
      "Batching analyze() + grep() in one QuickJS turn (40 KB cap).",
      "Patching cancellation arm in crates/mehh-agent/src/agent_loop.rs.",
    ],
    toolCalls: [
      {
        toolName: "codemode",
        argsSummary: "2 tools in 1 turn",
        meta: "rquickjs · 180ms",
        detail:
          'tools.analyze({ path: "crates/mehh-agent/src/agent_loop.rs", mode: "symbols" }) + tools.grep({ pattern: "is_cancelled", path: "crates/mehh-agent" })',
      },
      {
        toolName: "edit",
        argsSummary: "crates/mehh-agent/src/agent_loop.rs",
        meta: "+1 -1",
        diffLines: [
          { kind: "del", text: "- if cancel.is_cancelled() { break; }" },
          {
            kind: "add",
            text: "+ if cancel.is_cancelled() { set.abort_all(); break; }",
          },
        ],
      },
    ],
    assistantLines: [
      "Scanned public symbols in 1 turn and updated cancellation to abort active tasks.",
    ],
    copyCommand: "mehh --codemode",
    thinkingStatus: "high",
    ctxPct: 14,
    usageSummary: "11.8k in / 310 out",
    costSummary: "$0.0164",
  },
  {
    id: "lead_worker",
    tabLabel: "Lead / Worker",
    modelLabel: "Claude Sonnet 4.5",
    providerSlug: "anthropic",
    modeTag: "[smart]",
    sessionId: "9b20e4f7",
    cwd: "D:/Mehhcode",
    userPrompt: "tune BM25 tool_search weights and run crate tests",
    thoughtSummary: "Thought for 1.8s (2 lines · Ctrl+T to expand)",
    thoughtExpandedLines: [
      "Turns 0..1 [Lead]: claude-sonnet-4-5 set name=3x, schema=1x BM25 boost.",
      "Turns 2..3 [Worker]: gemini-2.5-flash edited tool_search.rs & ran cargo test.",
    ],
    toolCalls: [
      {
        toolName: "bash",
        argsSummary: '"cargo test -p mehh-coding-agent tool_search"',
        meta: "1.4s",
        detail: "test tool_search::tests::camel_and_snake_bm25 ... ok",
      },
    ],
    assistantLines: [
      "Lead planned in 2 turns; Worker applied edits and passed tests (0 rescues).",
    ],
    copyCommand:
      "mehh -m anthropic/claude-sonnet-4-5 --worker-model google/gemini-2.5-flash --lead-turns 2",
    thinkingStatus: "medium",
    ctxPct: 18,
    usageSummary: "14.6k in / 380 out",
    costSummary: "$0.0089",
  },
]

export const SEARCH_PIPELINE_MODES: ReadonlyArray<SearchPipelineMode> = [
  {
    id: "rust_crates",
    label: "Rust & Crates",
    queryExample: '"tokio JoinSet abort_all"',
    stage1Sources:
      "DuckDuckGo + Bing + crates.io (+ GitHub on repo/cli intent, max 5)",
    stage1Budget: "3.5s cap · 700ms straggler grace · RRF k=60 fusion",
    stage2Enrich: "Top 3 URLs -> <= 900-char Markdown passages (LRU cached)",
    routerChain: "Primary: MehhNative in-process metasearch (keyless)",
  },
  {
    id: "errors_tracebacks",
    label: "Errors & Tracebacks",
    queryExample: '"E0308 mismatched types async trait"',
    stage1Sources: "DuckDuckGo + Bing + StackOverflow (weight 0.95)",
    stage1Budget: "classify_query detects E0xxx/panics and merges SO + web",
    stage2Enrich: "Strips tracking params, extracts query-matched passages",
    routerChain: "Single-flight InFlightSlot deduplicates concurrent queries",
  },
  {
    id: "js_ts",
    label: "JS / TypeScript",
    queryExample: '"vite react-router v7 tsconfig"',
    stage1Sources: "DuckDuckGo + Bing + npm registry JSON API",
    stage1Budget: "Parallel JoinSet fetch with biased CancellationToken abort",
    stage2Enrich: "Skips snippets >= 500 chars and binary extensions",
    routerChain: "15m query memoization (128 entries) inside WebRouter",
  },
  {
    id: "failover",
    label: "7-Provider Failover",
    queryExample: "WebProviderChoice::Auto",
    stage1Sources:
      "Mehh -> Exa -> Parallel -> Firecrawl -> Tavily -> Keenable -> TinyFish",
    stage1Budget: "7s attempt timeout · 30s/60s/300s WebRouter provider cooldowns",
    stage2Enrich: "web_fetch fails over across Exa, Parallel, Firecrawl, Keenable",
    routerChain: "Zero API keys needed across all 7 search providers",
  },
]

export const CONTEXT_ENGINE_TABS: ReadonlyArray<ContextEngineTab> = [
  {
    id: "codemode",
    label: "QuickJS Codemode",
    sourceFile: "crates/mehh-codemode/src/tool.rs",
    summary:
      "Runs multi-tool JS scripts in one rquickjs sandbox turn (40 KB cap).",
    codeLines: [
      'const syms = await tools.analyze({ path: "crates/mehh-agent/src/agent_loop.rs", mode: "symbols" });',
      'const refs = await tools.grep({ pattern: "CancellationToken", path: "crates/mehh-agent" });',
      'store("snapshot", { syms, refs });',
      'return load("snapshot");',
    ],
    copyText:
      'const syms = await tools.analyze({ path: "crates/mehh-agent/src/agent_loop.rs", mode: "symbols" });\nconst refs = await tools.grep({ pattern: "CancellationToken", path: "crates/mehh-agent" });\nstore("snapshot", { syms, refs });\nreturn load("snapshot");',
    copyLabel: "Copy QuickJS codemode script",
    footnote:
      "Exposes tools.<name>(), searchTools(), describeTool(), and models.list()",
  },
  {
    id: "bm25_tools",
    label: "BM25 Tool Search",
    sourceFile: "crates/mehh-coding-agent/src/tool_search.rs",
    summary:
      "Defers heavy MCP schemas and activates matches on demand via BM25.",
    codeLines: [
      "// Weights: tool.name = 3x, description + schema = 1x",
      "// Tokenizes CamelCase & snake_case -> dynamic_tools",
      'await tools.tool_search({ query: "postgres schema query", activate: true });',
    ],
    copyText:
      'await tools.tool_search({ query: "postgres schema query", activate: true });',
    copyLabel: "Copy BM25 tool_search call",
    footnote: "Prevents dozens of MCP schemas from crowding active context",
  },
  {
    id: "analyze_lead_worker",
    label: "Analyze & Lead/Worker",
    sourceFile: "crates/mehh-tools/src/analyze.rs · lead_worker.rs",
    summary:
      "Brace-aware symbol spans + Lead planning / Worker execution split.",
    codeLines: [
      "// Brace-aware spans for Rust, TS/JS, Python, Go",
      "mehh -m anthropic/claude-sonnet-4-5 \\",
      "  --worker-model google/gemini-2.5-flash --lead-turns 2",
    ],
    copyText:
      "mehh -m anthropic/claude-sonnet-4-5 --worker-model google/gemini-2.5-flash --lead-turns 2",
    copyLabel: "Copy Lead/Worker command",
    footnote: "Auto-grants a 2-turn Lead rescue window after >= 2 tool errors",
  },
]

export const ARCHITECTURE_LAYERS: ReadonlyArray<ArchitectureLayer> = [
  {
    layer: "L4",
    name: "Binary",
    crates: [{ name: "mehh-cli", role: "CLI flags, mode routing, bootstrap" }],
  },
  {
    layer: "L3",
    name: "Orchestrator",
    crates: [
      {
        name: "mehh-coding-agent",
        role: "JSONL v3 sessions, 30 slash commands, RPC, Lead/Worker, BM25",
      },
    ],
  },
  {
    layer: "L2",
    name: "Capabilities",
    crates: [
      {
        name: "mehh-tools",
        role: "MehhNative search, Auto-Enrich, analyze, undo_edit, grep/bash",
      },
      { name: "mehh-codemode", role: "Embedded QuickJS (rquickjs) sandbox" },
      { name: "mehh-mcp", role: "Stdio & streamable HTTP MCP + OAuth" },
    ],
  },
  {
    layer: "L1",
    name: "Core Loop",
    crates: [
      { name: "mehh-agent", role: "Turn loop, tool dispatch, steering" },
    ],
  },
  {
    layer: "L0",
    name: "Foundations",
    crates: [
      { name: "mehh-ai", role: "Streaming LLM adapters, catalog, OAuth" },
      {
        name: "mehh-tui",
        role: "Crossterm renderer, markdown, editor, PasteBurst",
      },
      { name: "mehh-execpolicy", role: "Shell tokenizer & prefix policy" },
      { name: "mehh-sandbox", role: "bwrap / Seatbelt sandbox & env scrub" },
    ],
  },
]

export const COMPANION_PROJECTS: ReadonlyArray<CompanionProject> = [
  {
    id: "kbrowser",
    index: "01",
    name: "Kbrowser",
    tagline: "Multi-tab Rust browser on Tauri & WebView2",
    summary:
      "Windows 10/11 (x64) browser in Rust + Tauri + WebView2 with Fluent 2 and Win11 Mica styling.",
    repoUrl: "https://github.com/karmavn/Kbrowser",
    stack: ["Rust", "Tauri", "WebView2", "Fluent 2 · Mica"],
    highlights: [
      "Chrome extensions via about:extensions or Chrome Web Store navbar button",
      "Optional history suggestions and tab session persistence across reboots",
      "CLI launch (Kbrowser.exe example.com) and DevTools via Ctrl+Shift+I",
    ],
    accentLabel: "Desktop Browser",
  },
  {
    id: "optimum-geogebra",
    index: "02",
    name: "Optimum-Geogebra",
    tagline: "Offline GeoGebra Classic + 3D Geometry MCP",
    summary:
      "Electron app bundling offline GeoGebra Classic (geogebra-offline/), Quick Rename, and a 3D pyramid MCP server.",
    repoUrl: "https://github.com/karmavn/Optimum-Geogebra",
    stack: ["Electron", "JavaScript", "MCP Server", "Offline Engine"],
    highlights: [
      "draw_pyramid supports triangle to trapezoid bases with SA/SO/SH altitudes",
      "geogebra_eval & geogebra_screenshot auto-launch the app and capture canvas views",
      "Right-click any canvas point for Quick Rename, 100% offline",
    ],
    accentLabel: "AI + Math Tooling",
  },
  {
    id: "karmalaucher",
    index: "03",
    name: "Karma Launcher",
    tagline: "Android 16 Launcher3 + Material 3 Expressive",
    summary:
      "Open-source Android launcher on Launcher3 & Lawnchair with Material 3 Expressive and a geometric \"K\" icon.",
    repoUrl: "https://github.com/karmavn/karmalaucher",
    stack: ["Java", "Android 16 Launcher3", "Material 3 Expressive"],
    highlights: [
      "5×5 workspace & drawer, Android 13+ Themed Icons, custom fonts",
      "At a Glance + Smartspacer widget and QuickSwitch on Android 15–16",
      "Automated APK builds via GitHub Releases and nightly.link",
    ],
    accentLabel: "Mobile Launcher",
  },
]
