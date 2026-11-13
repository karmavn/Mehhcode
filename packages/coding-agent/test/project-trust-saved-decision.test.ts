import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { createProjectTrustContext } from "../src/cli/project-trust.ts";
import { resolveProjectTrusted } from "../src/core/project-trust.ts";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { createAgentSession } from "../src/core/sdk.ts";
import { SessionManager } from "../src/core/session-manager.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { ProjectTrustStore } from "../src/core/trust-manager.ts";

const tempDirs: string[] = [];

afterEach(() => {
	for (const dir of tempDirs.splice(0)) {
		rmSync(dir, { recursive: true, force: true });
	}
});

interface Dirs {
	agentDir: string;
	projectDir: string;
	probeFile: string;
}

function setup(): Dirs {
	const tempRoot = realpathSync(mkdtempSync(join(tmpdir(), "mehh-project-trust-saved-")));
	tempDirs.push(tempRoot);
	const dirs = {
		agentDir: join(tempRoot, "agent"),
		projectDir: join(tempRoot, "project"),
		probeFile: join(tempRoot, "probe.json"),
	};
	mkdirSync(dirs.agentDir, { recursive: true });
	mkdirSync(dirs.projectDir, { recursive: true });
	// AGENTS.md alone is not a trust-requiring project resource.
	writeFileSync(join(dirs.projectDir, "AGENTS.md"), "# untrusted\n");
	return dirs;
}

async function probeProjectTrust(dirs: Dirs): Promise<boolean> {
	const trustStore = new ProjectTrustStore(dirs.agentDir);
	// A project without trust-requiring resources honors the saved decision, or defaults to trusted.
	const projectTrusted = trustStore.get(dirs.projectDir) ?? true;
	const settingsManager = SettingsManager.create(dirs.projectDir, dirs.agentDir, { projectTrusted });
	const resourceLoader = new DefaultResourceLoader({
		cwd: dirs.projectDir,
		agentDir: dirs.agentDir,
		settingsManager,
		extensionFactories: [
			(mehh) => {
				mehh.on("session_start", (_event, ctx) => {
					writeFileSync(dirs.probeFile, JSON.stringify({ trusted: ctx.isProjectTrusted() }));
				});
			},
		],
		noSkills: true,
		noPromptTemplates: true,
		noThemes: true,
		noContextFiles: true,
	});
	await resourceLoader.reload();
	const { session } = await createAgentSession({
		cwd: dirs.projectDir,
		agentDir: dirs.agentDir,
		settingsManager,
		resourceLoader,
		sessionManager: SessionManager.inMemory(dirs.projectDir),
	});
	await session.bindExtensions({});
	try {
		return (JSON.parse(readFileSync(dirs.probeFile, "utf8")) as { trusted: boolean }).trusted;
	} finally {
		session.dispose();
	}
}

// Regression tests for https://github.com/mehh-code/mehh-code/issues/10947
describe("saved project trust for projects without trust-requiring resources", () => {
	it("reports a saved Do not trust decision to extensions", async () => {
		const dirs = setup();
		new ProjectTrustStore(dirs.agentDir).set(dirs.projectDir, false);

		expect(await probeProjectTrust(dirs)).toBe(false);
	}, 30_000);

	it("auto-trusts when no decision is saved", async () => {
		const dirs = setup();

		expect(await probeProjectTrust(dirs)).toBe(true);
	}, 30_000);

	it("resolveProjectTrusted honors the saved decision before auto-trust", async () => {
		const dirs = setup();
		const trustStore = new ProjectTrustStore(dirs.agentDir);
		const resolve = () =>
			resolveProjectTrusted({
				cwd: dirs.projectDir,
				trustStore,
				projectTrustContext: createProjectTrustContext({
					cwd: dirs.projectDir,
					mode: "print",
					settingsManager: SettingsManager.inMemory(),
					hasUI: false,
				}),
			});

		expect(await resolve()).toBe(true);
		trustStore.set(dirs.projectDir, false);
		expect(await resolve()).toBe(false);
		trustStore.set(dirs.projectDir, true);
		expect(await resolve()).toBe(true);
	});
});
