import { existsSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getModel } from "@mehh-code/ai/compat";
import type { TUI } from "@mehh-code/tui";
import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import builtInToolRenderer from "../examples/extensions/built-in-tool-renderer.ts";
import minimalMode from "../examples/extensions/minimal-mode.ts";
import type { InlineExtension, ToolDefinition } from "../src/core/extensions/types.ts";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { createAgentSession } from "../src/core/sdk.ts";
import { SessionManager } from "../src/core/session-manager.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { ToolExecutionComponent } from "../src/modes/interactive/components/tool-execution.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";

describe("tool renderer examples", () => {
	let tempDir: string;
	let agentDir: string;

	beforeAll(() => {
		initTheme("dark");
	});

	beforeEach(() => {
		tempDir = join(tmpdir(), `mehh-tool-renderer-example-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		agentDir = join(tempDir, "agent");
		mkdirSync(agentDir, { recursive: true });
	});

	afterEach(() => {
		if (tempDir && existsSync(tempDir)) {
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	async function getSessionState(factory: InlineExtension | undefined, tools: string[]) {
		const settingsManager = SettingsManager.inMemory();
		const resourceLoader = new DefaultResourceLoader({
			cwd: tempDir,
			agentDir,
			settingsManager,
			noSkills: true,
			noPromptTemplates: true,
			noThemes: true,
			noContextFiles: true,
			extensionFactories: factory ? [factory] : [],
		});
		await resourceLoader.reload();
		expect(resourceLoader.getExtensions().errors).toEqual([]);
		const { session } = await createAgentSession({
			cwd: tempDir,
			agentDir,
			model: getModel("anthropic", "claude-sonnet-4-5"),
			settingsManager,
			sessionManager: SessionManager.inMemory(tempDir),
			resourceLoader,
			tools,
		});
		try {
			return {
				systemPrompt: session.systemPrompt,
				editToolDefinition: session.getToolDefinition("edit"),
			};
		} finally {
			session.dispose();
		}
	}

	it.each([
		{
			name: "built-in tool renderer",
			factory: builtInToolRenderer,
			tools: ["read", "bash", "edit", "write"],
		},
		{
			name: "minimal mode",
			factory: minimalMode,
			tools: ["read", "bash", "write", "edit", "find", "grep", "ls"],
		},
	])("keeps the system prompt unchanged for the $name example", async ({ factory, tools }) => {
		// Regression test for https://github.com/mehh-code/mehh-code/issues/10072
		const baseline = await getSessionState(undefined, tools);
		const withRenderer = await getSessionState(factory, tools);

		expect(withRenderer.systemPrompt).toBe(baseline.systemPrompt);
	});

	it("keeps minimal mode's edit tool in the default shell", async () => {
		// Regression test for https://github.com/mehh-code/mehh-code/issues/10072
		const result = await getSessionState(minimalMode, ["edit"]);
		const editToolDefinition = result.editToolDefinition;
		if (!editToolDefinition) throw new Error("minimal mode did not register the edit tool");
		const { renderShell: _renderShell, ...defaultShellDefinition } = editToolDefinition;

		expect(renderEditTool(editToolDefinition, tempDir)).toEqual(renderEditTool(defaultShellDefinition, tempDir));
	});
});

function renderEditTool(definition: ToolDefinition, cwd: string): string[] {
	const component = new ToolExecutionComponent(
		"edit",
		"edit-shell-test",
		{ path: "notes.txt", oldText: "before", newText: "after" },
		{},
		definition,
		{ requestRender() {} } as unknown as TUI,
		cwd,
	);
	return component.render(40);
}
