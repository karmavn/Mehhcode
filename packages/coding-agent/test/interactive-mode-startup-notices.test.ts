import { Container } from "@mehh-code/tui";
import { beforeAll, describe, expect, test } from "vitest";
import { ExtensionRunner } from "../src/core/extensions/runner.ts";
import type { Extension, MarkdownMessageType, MarkdownTransformer } from "../src/core/extensions/types.ts";
import { InMemorySettingsStorage, SettingsManager } from "../src/core/settings-manager.ts";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";

type StartupNoticesContext = {
	startupNoticesShown: boolean;
	changelogMarkdown: string | undefined;
	version: string;
	chatContainer: Container;
	settingsManager: SettingsManager;
	mermaidMarkdownTransformer: MarkdownTransformer;
	session: { extensionRunner: { getMarkdownTransformers(): MarkdownTransformer[] } };
};

const prototype = InteractiveMode.prototype as unknown as {
	showStartupNoticesIfNeeded(this: StartupNoticesContext): void;
};

describe("InteractiveMode startup notices", () => {
	beforeAll(() => initTheme("dark"));

	function renderNotices(extensions: Pick<Extension, "markdownTransformer">[]): string {
		// getMarkdownTransformers only reads the extensions' transformer registrations.
		const runner = { extensions } as unknown as ExtensionRunner;
		const context: StartupNoticesContext = {
			startupNoticesShown: false,
			changelogMarkdown: "## [2.0.0]\n\n- :warning: Mehh change",
			version: "2.0.0",
			chatContainer: new Container(),
			settingsManager: SettingsManager.fromStorage(new InMemorySettingsStorage()),
			mermaidMarkdownTransformer: (markdown) => markdown,
			session: {
				extensionRunner: {
					getMarkdownTransformers: () => ExtensionRunner.prototype.getMarkdownTransformers.call(runner),
				},
			},
		};
		Object.setPrototypeOf(context, InteractiveMode.prototype);
		prototype.showStartupNoticesIfNeeded.call(context);
		return stripAnsi(context.chatContainer.render(120).join("\n"));
	}

	function extensionWithTransformer(
		transformer: MarkdownTransformer,
		messageTypes: readonly MarkdownMessageType[],
	): Pick<Extension, "markdownTransformer"> {
		return { markdownTransformer: { transformer, messageTypes: new Set(messageTypes) } };
	}

	const replaceShortcodes: MarkdownTransformer = (markdown) => markdown.replaceAll(":warning:", "⚠");

	test("runs changelog transformers on What's New", () => {
		const rendered = renderNotices([extensionWithTransformer(replaceShortcodes, ["changelog"])]);

		expect(rendered).toContain("⚠ Mehh change");
		expect(rendered).not.toContain(":warning:");
	});

	test("skips transformers registered for the default message types", () => {
		const rendered = renderNotices([
			extensionWithTransformer(replaceShortcodes, ["user", "assistant", "assistant-thinking"]),
		]);

		expect(rendered).toContain(":warning: Mehh change");
	});
});
