import { EventEmitter } from "node:events";
import { existsSync, mkdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { PassThrough } from "node:stream";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DefaultPackageManager, type ProgressEvent, type ResolvedResource } from "../src/core/package-manager.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";

function normalizeForMatch(value: string): string {
	return value.replace(/\\/g, "/");
}

function pathEndsWith(actualPath: string, suffix: string): boolean {
	return normalizeForMatch(actualPath).endsWith(normalizeForMatch(suffix));
}

class MockSpawnedProcess extends EventEmitter {
	stdout = new PassThrough();
	stderr = new PassThrough();

	kill(): boolean {
		this.emit("close", null, "SIGTERM");
		return true;
	}
}

interface PackageManagerInternals {
	runCommand(command: string, args: string[], options?: { cwd?: string }): Promise<void>;
	getPackageManagerName(): string;
	getGitDependencyInstallArgs(): string[];
	runCommandCapture(
		command: string,
		args: string[],
		options?: { cwd?: string; timeoutMs?: number; env?: Record<string, string> },
	): Promise<string>;
	getLocalGitUpdateTarget(installedPath: string): Promise<{ ref: string; head: string; fetchArgs: string[] }>;
	parseSource(
		source: string,
	):
		| { type: "npm"; spec: string; name: string; pinned: boolean }
		| { type: "git"; repo: string; host: string; path: string; pinned: boolean; ref?: string }
		| { type: "local"; path: string };
	getNpmInstallPath(
		source: { type: "npm"; spec: string; name: string; pinned: boolean },
		scope: "user" | "project",
	): string;
	getGitInstallPath(
		source: { type: "git"; repo: string; host: string; path: string; pinned: boolean; ref?: string },
		scope: "user" | "project",
	): string;
}

// Helper to check if a resource is enabled
const isEnabled = (r: ResolvedResource, pathMatch: string, matchFn: "endsWith" | "includes" = "endsWith") => {
	const normalizedPath = normalizeForMatch(r.path);
	const normalizedMatch = normalizeForMatch(pathMatch);
	return matchFn === "endsWith"
		? normalizedPath.endsWith(normalizedMatch) && r.enabled
		: normalizedPath.includes(normalizedMatch) && r.enabled;
};

const isDisabled = (r: ResolvedResource, pathMatch: string, matchFn: "endsWith" | "includes" = "endsWith") => {
	const normalizedPath = normalizeForMatch(r.path);
	const normalizedMatch = normalizeForMatch(pathMatch);
	return matchFn === "endsWith"
		? normalizedPath.endsWith(normalizedMatch) && !r.enabled
		: normalizedPath.includes(normalizedMatch) && !r.enabled;
};

describe("DefaultPackageManager", () => {
	let tempDir: string;
	let agentDir: string;
	let settingsManager: SettingsManager;
	let packageManager: DefaultPackageManager;
	let previousOfflineEnv: string | undefined;

	beforeEach(() => {
		previousOfflineEnv = process.env.MEHH_OFFLINE;
		delete process.env.MEHH_OFFLINE;
		tempDir = join(tmpdir(), `pm-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
		agentDir = join(tempDir, "agent");
		mkdirSync(agentDir, { recursive: true });

		settingsManager = SettingsManager.inMemory();
		packageManager = new DefaultPackageManager({
			cwd: tempDir,
			agentDir,
			settingsManager,
		});
	});

	afterEach(() => {
		if (previousOfflineEnv === undefined) {
			delete process.env.MEHH_OFFLINE;
		} else {
			process.env.MEHH_OFFLINE = previousOfflineEnv;
		}
		vi.restoreAllMocks();
		vi.unstubAllGlobals();
		rmSync(tempDir, { recursive: true, force: true });
	});

	describe("resolve", () => {
		it("should return no package-sourced paths when no sources configured", async () => {
			const result = await packageManager.resolve();
			expect(result.extensions).toEqual([]);
			expect(result.prompts).toEqual([]);
			expect(result.themes).toEqual([]);
			expect(result.skills.every((r) => r.metadata.source === "auto" && r.metadata.origin === "top-level")).toBe(
				true,
			);
		});

		it("should resolve built-in extensions with user exclusions and project overrides", async () => {
			const pm = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
				builtinExtensions: ["mcp", "llama.cpp"],
			});
			const builtins = async () =>
				(await pm.resolve()).extensions.map((r) => [r.path, r.enabled, r.metadata.source, r.metadata.scope]);

			expect(await builtins()).toEqual([
				["builtin:mcp", true, "builtin", "user"],
				["builtin:llama.cpp", true, "builtin", "user"],
			]);

			settingsManager.setExtensionPaths(["-builtin:mcp"]);
			settingsManager.setProjectExtensionPaths(["+builtin:mcp", "-builtin:llama.cpp"]);
			expect(await builtins()).toEqual([
				["builtin:mcp", true, "builtin", "project"],
				["builtin:llama.cpp", false, "builtin", "project"],
			]);

			settingsManager.setProjectExtensionPaths([]);
			expect(await builtins()).toEqual([
				["builtin:mcp", false, "builtin", "user"],
				["builtin:llama.cpp", true, "builtin", "user"],
			]);
		});

		it("should resolve skill paths from settings", async () => {
			const skillDir = join(agentDir, "skills", "my-skill");
			mkdirSync(skillDir, { recursive: true });
			const skillFile = join(skillDir, "SKILL.md");
			writeFileSync(
				skillFile,
				`---
name: test-skill
description: A test skill
---
Content`,
			);

			settingsManager.setSkillPaths(["skills"]);

			const result = await packageManager.resolve();
			// Skills with SKILL.md are returned as file paths
			expect(result.skills.some((r) => r.path === skillFile && r.enabled)).toBe(true);
		});

		it("should auto-discover root markdown skills from .mehh skill dirs", async () => {
			const skillFile = join(agentDir, "skills", "single-file.md");
			mkdirSync(join(agentDir, "skills"), { recursive: true });
			writeFileSync(
				skillFile,
				`---
name: single-file
description: A root markdown skill
---
Content`,
			);

			const result = await packageManager.resolve();
			expect(result.skills.some((r) => r.path === skillFile && r.enabled)).toBe(true);
		});

		it("should auto-discover user prompts with overrides", async () => {
			const promptsDir = join(agentDir, "prompts");
			mkdirSync(promptsDir, { recursive: true });
			const promptPath = join(promptsDir, "auto.md");
			writeFileSync(promptPath, "Auto prompt");

			settingsManager.setPromptTemplatePaths(["!prompts/auto.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => r.path === promptPath && !r.enabled)).toBe(true);
		});

		it("should resolve symlinked user and project resources once", async () => {
			const previousHome = process.env.HOME;
			process.env.HOME = tempDir;

			try {
				const sharedDir = join(tempDir, "shared-resources");
				const sharedSkillsDir = join(sharedDir, "skills");
				const sharedPromptsDir = join(sharedDir, "prompts");
				const sharedThemesDir = join(sharedDir, "themes");
				mkdirSync(sharedSkillsDir, { recursive: true });
				mkdirSync(sharedPromptsDir, { recursive: true });
				mkdirSync(sharedThemesDir, { recursive: true });

				mkdirSync(join(sharedSkillsDir, "shared-skill"), { recursive: true });
				writeFileSync(
					join(sharedSkillsDir, "shared-skill", "SKILL.md"),
					`---
name: shared-skill
description: Shared skill
---
Content`,
				);
				writeFileSync(join(sharedPromptsDir, "shared.md"), "Shared prompt");
				writeFileSync(join(sharedThemesDir, "shared.json"), JSON.stringify({ name: "shared-theme" }));

				mkdirSync(join(agentDir), { recursive: true });
				mkdirSync(join(tempDir, ".mehh"), { recursive: true });
				symlinkSync(sharedSkillsDir, join(agentDir, "skills"), "dir");
				symlinkSync(sharedPromptsDir, join(agentDir, "prompts"), "dir");
				symlinkSync(sharedThemesDir, join(agentDir, "themes"), "dir");
				symlinkSync(sharedSkillsDir, join(tempDir, ".mehh", "skills"), "dir");
				symlinkSync(sharedPromptsDir, join(tempDir, ".mehh", "prompts"), "dir");
				symlinkSync(sharedThemesDir, join(tempDir, ".mehh", "themes"), "dir");

				const result = await packageManager.resolve();

				expect({
					skills: result.skills.length,
					prompts: result.prompts.length,
					themes: result.themes.length,
				}).toEqual({
					skills: 1,
					prompts: 1,
					themes: 1,
				});

				// Project auto-discovered has higher precedence than user auto-discovered,
				// so the surviving entry should be scoped to project.
				expect(result.skills[0].metadata.scope).toBe("project");
				expect(result.prompts[0].metadata.scope).toBe("project");
				expect(result.themes[0].metadata.scope).toBe("project");
			} finally {
				if (previousHome === undefined) {
					delete process.env.HOME;
				} else {
					process.env.HOME = previousHome;
				}
			}
		});

		it("should auto-discover project prompts with overrides", async () => {
			const promptsDir = join(tempDir, ".mehh", "prompts");
			mkdirSync(promptsDir, { recursive: true });
			const promptPath = join(promptsDir, "is.md");
			writeFileSync(promptPath, "Is prompt");

			settingsManager.setProjectPromptTemplatePaths(["!prompts/is.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => r.path === promptPath && !r.enabled)).toBe(true);
		});

		it("should ignore executable package entries but load data resources", async () => {
			// A mixed package declares an extension entry next to its data resources.
			const pkgDir = join(tempDir, "mixed-pkg");
			mkdirSync(join(pkgDir, "extensions"), { recursive: true });
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(
				join(pkgDir, "package.json"),
				JSON.stringify({
					name: "mixed-pkg",
					mehh: {
						extensions: ["./extensions/clip.ts"],
						prompts: ["./prompts"],
					},
				}),
			);
			writeFileSync(join(pkgDir, "extensions", "clip.ts"), "export default function() {}");
			writeFileSync(join(pkgDir, "prompts", "review.md"), "Review");

			settingsManager.setPackages([pkgDir]);

			const result = await packageManager.resolve();

			expect(result.extensions).toEqual([]);
			expect(result.prompts.some((r) => r.path === join(pkgDir, "prompts", "review.md") && r.enabled)).toBe(true);
		});
	});

	describe("auto-discovered skill metadata", () => {
		it("should use the agent dir as baseDir for user .mehh/agent skills", async () => {
			const skillPath = join(agentDir, "skills", "user-mehh", "SKILL.md");
			mkdirSync(join(agentDir, "skills", "user-mehh"), { recursive: true });
			writeFileSync(skillPath, "---\nname: user-mehh\ndescription: user mehh\n---\n");

			const result = await packageManager.resolve();
			const skill = result.skills.find((r) => r.path === skillPath);

			expect(skill?.metadata.source).toBe("auto");
			expect(skill?.metadata.scope).toBe("user");
			expect(skill?.metadata.baseDir).toBe(agentDir);
		});

		it("should use the project .mehh dir as baseDir for project .mehh skills", async () => {
			const projectBaseDir = join(tempDir, ".mehh");
			const skillPath = join(projectBaseDir, "skills", "project-mehh", "SKILL.md");
			mkdirSync(join(projectBaseDir, "skills", "project-mehh"), { recursive: true });
			writeFileSync(skillPath, "---\nname: project-mehh\ndescription: project mehh\n---\n");

			const result = await packageManager.resolve();
			const skill = result.skills.find((r) => r.path === skillPath);

			expect(skill?.metadata.source).toBe("auto");
			expect(skill?.metadata.scope).toBe("project");
			expect(skill?.metadata.baseDir).toBe(projectBaseDir);
		});

		it("should use ~/.agents as baseDir for user .agents skills", async () => {
			const previousHome = process.env.HOME;
			process.env.HOME = tempDir;

			try {
				const agentsBaseDir = join(tempDir, ".agents");
				const skillPath = join(agentsBaseDir, "skills", "user-agents", "SKILL.md");
				mkdirSync(join(agentsBaseDir, "skills", "user-agents"), { recursive: true });
				writeFileSync(skillPath, "---\nname: user-agents\ndescription: user agents\n---\n");

				const result = await packageManager.resolve();
				const skill = result.skills.find((r) => r.path === skillPath);

				expect(skill?.metadata.source).toBe("auto");
				expect(skill?.metadata.scope).toBe("user");
				expect(skill?.metadata.baseDir).toBe(agentsBaseDir);
			} finally {
				if (previousHome === undefined) {
					delete process.env.HOME;
				} else {
					process.env.HOME = previousHome;
				}
			}
		});

		it("should use each project .agents dir as baseDir for project .agents skills", async () => {
			const repoRoot = join(tempDir, "repo");
			const nestedCwd = join(repoRoot, "packages", "feature");
			mkdirSync(nestedCwd, { recursive: true });
			mkdirSync(join(repoRoot, ".git"), { recursive: true });

			const repoAgentsBaseDir = join(repoRoot, ".agents");
			const repoSkill = join(repoAgentsBaseDir, "skills", "repo", "SKILL.md");
			mkdirSync(join(repoAgentsBaseDir, "skills", "repo"), { recursive: true });
			writeFileSync(repoSkill, "---\nname: repo\ndescription: repo\n---\n");

			const packageAgentsBaseDir = join(repoRoot, "packages", ".agents");
			const packageSkill = join(packageAgentsBaseDir, "skills", "package", "SKILL.md");
			mkdirSync(join(packageAgentsBaseDir, "skills", "package"), { recursive: true });
			writeFileSync(packageSkill, "---\nname: package\ndescription: package\n---\n");

			const pm = new DefaultPackageManager({
				cwd: nestedCwd,
				agentDir,
				settingsManager,
			});

			const result = await pm.resolve();
			const resolvedRepoSkill = result.skills.find((r) => r.path === repoSkill);
			const resolvedPackageSkill = result.skills.find((r) => r.path === packageSkill);

			expect(resolvedRepoSkill?.metadata.source).toBe("auto");
			expect(resolvedRepoSkill?.metadata.scope).toBe("project");
			expect(resolvedRepoSkill?.metadata.baseDir).toBe(repoAgentsBaseDir);
			expect(resolvedPackageSkill?.metadata.source).toBe("auto");
			expect(resolvedPackageSkill?.metadata.scope).toBe("project");
			expect(resolvedPackageSkill?.metadata.baseDir).toBe(packageAgentsBaseDir);
		});
	});

	describe(".agents/skills auto-discovery", () => {
		it("should scan .agents/skills from cwd up to git repo root", async () => {
			const repoRoot = join(tempDir, "repo");
			const nestedCwd = join(repoRoot, "packages", "feature");
			mkdirSync(nestedCwd, { recursive: true });
			mkdirSync(join(repoRoot, ".git"), { recursive: true });

			const aboveRepoSkill = join(tempDir, ".agents", "skills", "above-repo", "SKILL.md");
			mkdirSync(join(tempDir, ".agents", "skills", "above-repo"), { recursive: true });
			writeFileSync(aboveRepoSkill, "---\nname: above-repo\ndescription: above\n---\n");

			const repoRootSkill = join(repoRoot, ".agents", "skills", "repo-root", "SKILL.md");
			mkdirSync(join(repoRoot, ".agents", "skills", "repo-root"), { recursive: true });
			writeFileSync(repoRootSkill, "---\nname: repo-root\ndescription: repo\n---\n");

			const nestedSkill = join(repoRoot, "packages", ".agents", "skills", "nested", "SKILL.md");
			mkdirSync(join(repoRoot, "packages", ".agents", "skills", "nested"), { recursive: true });
			writeFileSync(nestedSkill, "---\nname: nested\ndescription: nested\n---\n");

			const pm = new DefaultPackageManager({
				cwd: nestedCwd,
				agentDir,
				settingsManager,
			});

			const result = await pm.resolve();
			expect(result.skills.some((r) => r.path === repoRootSkill && r.enabled)).toBe(true);
			expect(result.skills.some((r) => r.path === nestedSkill && r.enabled)).toBe(true);
			expect(result.skills.some((r) => r.path === aboveRepoSkill)).toBe(false);
		});

		it("should scan .agents/skills up to filesystem root when not in a git repo", async () => {
			const nonRepoRoot = join(tempDir, "non-repo");
			const nestedCwd = join(nonRepoRoot, "a", "b");
			mkdirSync(nestedCwd, { recursive: true });

			const rootSkill = join(nonRepoRoot, ".agents", "skills", "root", "SKILL.md");
			mkdirSync(join(nonRepoRoot, ".agents", "skills", "root"), { recursive: true });
			writeFileSync(rootSkill, "---\nname: root\ndescription: root\n---\n");

			const middleSkill = join(nonRepoRoot, "a", ".agents", "skills", "middle", "SKILL.md");
			mkdirSync(join(nonRepoRoot, "a", ".agents", "skills", "middle"), { recursive: true });
			writeFileSync(middleSkill, "---\nname: middle\ndescription: middle\n---\n");

			const pm = new DefaultPackageManager({
				cwd: nestedCwd,
				agentDir,
				settingsManager,
			});

			const result = await pm.resolve();
			expect(result.skills.some((r) => r.path === rootSkill && r.enabled)).toBe(true);
			expect(result.skills.some((r) => r.path === middleSkill && r.enabled)).toBe(true);
		});

		it("should ignore root markdown files in .agents/skills but discover nested markdown skills", async () => {
			const agentsSkillsDir = join(tempDir, ".agents", "skills");
			mkdirSync(join(agentsSkillsDir, "nested-skill"), { recursive: true });
			mkdirSync(join(agentsSkillsDir, "third-party"), { recursive: true });
			mkdirSync(join(agentsSkillsDir, "third-party", "vendor", "pack"), { recursive: true });
			const rootSkill = join(agentsSkillsDir, "root-file.md");
			const nestedSkill = join(agentsSkillsDir, "nested-skill", "SKILL.md");
			const nestedMarkdownSkill = join(agentsSkillsDir, "third-party", "child-skill.md");
			const deeplyNestedMarkdownSkill = join(agentsSkillsDir, "third-party", "vendor", "pack", "deep-skill.md");
			writeFileSync(rootSkill, "---\nname: root-file\ndescription: Root markdown file\n---\n");
			writeFileSync(nestedSkill, "---\nname: nested-skill\ndescription: Nested skill\n---\n");
			writeFileSync(nestedMarkdownSkill, "---\nname: child-skill\ndescription: Nested markdown skill\n---\n");
			writeFileSync(deeplyNestedMarkdownSkill, "---\nname: deep-skill\ndescription: Deep markdown skill\n---\n");

			const pm = new DefaultPackageManager({
				cwd: join(tempDir, "work"),
				agentDir,
				settingsManager,
			});
			mkdirSync(join(tempDir, "work"), { recursive: true });

			const result = await pm.resolve();
			expect(result.skills.some((r) => r.path === rootSkill)).toBe(false);
			expect(result.skills.some((r) => r.path === nestedSkill && r.enabled)).toBe(true);
			expect(result.skills.some((r) => r.path === nestedMarkdownSkill && r.enabled)).toBe(true);
			expect(result.skills.some((r) => r.path === deeplyNestedMarkdownSkill && r.enabled)).toBe(true);
		});

		it("should keep ~/.agents/skills user-scoped when cwd is under home in a non-git directory", async () => {
			const previousHome = process.env.HOME;
			process.env.HOME = tempDir;

			try {
				const cwd = join(tempDir, "scratch", "nested");
				const localAgentDir = join(tempDir, ".mehh", "agent");
				const localSettingsManager = SettingsManager.inMemory();
				mkdirSync(cwd, { recursive: true });
				mkdirSync(localAgentDir, { recursive: true });

				const homeSkill = join(tempDir, ".agents", "skills", "home-skill", "SKILL.md");
				mkdirSync(join(tempDir, ".agents", "skills", "home-skill"), { recursive: true });
				writeFileSync(homeSkill, "---\nname: home-skill\ndescription: home\n---\n");

				const pm = new DefaultPackageManager({
					cwd,
					agentDir: localAgentDir,
					settingsManager: localSettingsManager,
				});

				const result = await pm.resolve();
				const matchingSkills = result.skills.filter((r) => r.path === homeSkill);
				expect(matchingSkills).toHaveLength(1);
				expect(matchingSkills[0]?.enabled).toBe(true);
				expect(matchingSkills[0]?.metadata.scope).toBe("user");
				expect(matchingSkills[0]?.metadata.source).toBe("auto");
			} finally {
				if (previousHome === undefined) {
					delete process.env.HOME;
				} else {
					process.env.HOME = previousHome;
				}
			}
		});

		it("should dedupe user skill entries when ~/.mehh/agent/skills is a symlink to ~/.agents/skills", async () => {
			const previousHome = process.env.HOME;
			process.env.HOME = tempDir;

			try {
				const agentSkillsDir = join(agentDir, "skills");
				const agentsSkillsDir = join(tempDir, ".agents", "skills");
				mkdirSync(agentsSkillsDir, { recursive: true });
				// Use junction on Windows to avoid EPERM when symlink privileges are unavailable.
				const directoryLinkType = process.platform === "win32" ? "junction" : "dir";
				symlinkSync(agentsSkillsDir, agentSkillsDir, directoryLinkType);

				const skillPath = join(agentsSkillsDir, "foo", "SKILL.md");
				mkdirSync(join(agentsSkillsDir, "foo"), { recursive: true });
				writeFileSync(skillPath, "---\nname: foo\ndescription: foo\n---\n");

				const result = await packageManager.resolve();
				const fooSkills = result.skills.filter((r) => pathEndsWith(r.path, "foo/SKILL.md"));

				expect(fooSkills).toHaveLength(1);
			} finally {
				if (previousHome === undefined) {
					delete process.env.HOME;
				} else {
					process.env.HOME = previousHome;
				}
			}
		});
	});

	describe("ignore files", () => {
		it("should respect .gitignore in skill directories", async () => {
			const skillsDir = join(agentDir, "skills");
			mkdirSync(skillsDir, { recursive: true });
			writeFileSync(join(skillsDir, ".gitignore"), "venv\n__pycache__\n");

			const goodSkillDir = join(skillsDir, "good-skill");
			mkdirSync(goodSkillDir, { recursive: true });
			writeFileSync(join(goodSkillDir, "SKILL.md"), "---\nname: good-skill\ndescription: Good\n---\nContent");

			const ignoredSkillDir = join(skillsDir, "venv", "bad-skill");
			mkdirSync(ignoredSkillDir, { recursive: true });
			writeFileSync(join(ignoredSkillDir, "SKILL.md"), "---\nname: bad-skill\ndescription: Bad\n---\nContent");

			settingsManager.setSkillPaths(["skills"]);

			const result = await packageManager.resolve();
			expect(result.skills.some((r) => r.path.includes("good-skill") && r.enabled)).toBe(true);
			expect(result.skills.some((r) => r.path.includes("venv") && r.enabled)).toBe(false);
		});

		it("should not apply parent .gitignore to .mehh auto-discovery", async () => {
			writeFileSync(join(tempDir, ".gitignore"), ".mehh\n");

			const skillDir = join(tempDir, ".mehh", "skills", "auto-skill");
			mkdirSync(skillDir, { recursive: true });
			const skillPath = join(skillDir, "SKILL.md");
			writeFileSync(skillPath, "---\nname: auto-skill\ndescription: Auto\n---\nContent");

			const result = await packageManager.resolve();
			expect(result.skills.some((r) => r.path === skillPath && r.enabled)).toBe(true);
		});
	});

	describe("progress callback", () => {
		it("should emit progress events", async () => {
			const events: ProgressEvent[] = [];
			packageManager.setProgressCallback((event) => events.push(event));

			const pkgDir = join(tempDir, "progress-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "review.md"), "Review");

			// Local packages don't trigger install progress, but we can verify the callback is set
			settingsManager.setPackages([pkgDir]);
			await packageManager.resolve();

			// For now just verify no errors - npm/git would trigger actual events
			expect(events.length).toBe(0);
		});
	});

	describe("command spawning", () => {
		it("should preserve argv entries containing spaces", () => {
			const managerWithInternals = packageManager as unknown as {
				runCommandSync(command: string, args: string[]): string;
			};
			const valueWithSpace = "C:\\Users\\A B\\.mehh\\npm";
			const output = managerWithInternals.runCommandSync(process.execPath, [
				"-e",
				"console.log(process.argv[1])",
				valueWithSpace,
			]);

			expect(output).toBe(valueWithSpace);
		});
	});

	describe("npmCommand", () => {
		it("should use npmCommand argv for npm installs", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["mise", "exec", "node@20", "--", "npm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			await packageManager.install("npm:@scope/pkg");

			expect(runCommandSpy).toHaveBeenCalledWith(
				"mise",
				[
					"exec",
					"node@20",
					"--",
					"npm",
					"install",
					"@scope/pkg",
					"--prefix",
					realpathSync(join(agentDir, "npm")),
					"--legacy-peer-deps",
					"--no-audit",
					"--no-fund",
				],
				undefined,
			);
		});

		it("should pass legacy peer deps when uninstalling npm packages", async () => {
			mkdirSync(join(agentDir, "npm"), { recursive: true });
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			await packageManager.remove("npm:@scope/pkg");

			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				[
					"uninstall",
					"@scope/pkg",
					"--prefix",
					realpathSync(join(agentDir, "npm")),
					"--legacy-peer-deps",
					"--no-audit",
					"--no-fund",
				],
				undefined,
			);
		});

		// Regression test for #11358: npm writes nested package-lock.json entries for a symlinked --prefix
		it("should pass the real path as --prefix when the agent dir is a symlink", async () => {
			const realAgentDir = join(tempDir, "dotfiles", "agent");
			mkdirSync(realAgentDir, { recursive: true });
			const linkedAgentDir = join(tempDir, "linked-agent");
			symlinkSync(realAgentDir, linkedAgentDir, "dir");
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir: linkedAgentDir,
				settingsManager,
			});
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);
			const realNpmRoot = join(realpathSync(realAgentDir), "npm");

			await packageManager.install("npm:@scope/pkg");
			await packageManager.remove("npm:@scope/pkg");

			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["install", "@scope/pkg", "--prefix", realNpmRoot, "--legacy-peer-deps", "--no-audit", "--no-fund"],
				undefined,
			);
			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["uninstall", "@scope/pkg", "--prefix", realNpmRoot, "--legacy-peer-deps", "--no-audit", "--no-fund"],
				undefined,
			);
		});

		it("should use bun --cwd for npm package installs", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["mise", "exec", "bun@1", "--", "bun"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			await packageManager.install("npm:@scope/pkg");

			expect(runCommandSpy).toHaveBeenCalledWith(
				"mise",
				[
					"exec",
					"bun@1",
					"--",
					"bun",
					"install",
					"@scope/pkg",
					"--cwd",
					realpathSync(join(agentDir, "npm")),
					"--omit=peer",
				],
				undefined,
			);
		});

		it("should install git package dependencies without auto-installing peers", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			const runCommandSpy = vi
				.spyOn(packageManager as any, "runCommand")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					if (command === "git" && args[0] === "clone") {
						mkdirSync(targetDir, { recursive: true });
						writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
					}
				});

			await packageManager.install(source);

			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"],
				{
					cwd: targetDir,
				},
			);
		});

		it("should remove a newly created checkout when git clone fails", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			vi.spyOn(packageManager as any, "runCommand").mockImplementation(async (...callArgs: unknown[]) => {
				const [command, args] = callArgs as [string, string[]];
				if (command === "git" && args[0] === "clone") {
					mkdirSync(targetDir, { recursive: true });
					throw new Error("simulated git clone failure");
				}
			});

			await expect(packageManager.install(source)).rejects.toThrow("simulated git clone failure");

			expect(existsSync(targetDir)).toBe(false);
		});

		it("should remove a newly cloned checkout when dependency installation fails", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			vi.spyOn(packageManager as any, "runCommand").mockImplementation(async (...callArgs: unknown[]) => {
				const [command, args] = callArgs as [string, string[]];
				if (command === "git" && args[0] === "clone") {
					mkdirSync(targetDir, { recursive: true });
					writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
					return;
				}
				if (command === "npm") {
					throw new Error("simulated dependency install failure");
				}
			});

			await expect(packageManager.install(source)).rejects.toThrow("simulated dependency install failure");

			expect(existsSync(targetDir)).toBe(false);
		});

		it("should reconcile an existing git checkout to a pinned ref during install", async () => {
			const source = "git:github.com/user/repo@v2";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			mkdirSync(targetDir, { recursive: true });
			writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));

			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			vi.spyOn(managerWithInternals, "runCommandCapture").mockImplementation(async (_command, args) => {
				if (args[0] === "rev-parse" && args[1] === "HEAD") {
					return "old-head";
				}
				if (args[0] === "rev-parse" && args[1] === "FETCH_HEAD^{commit}") {
					return "new-head";
				}
				throw new Error(`Unexpected runCommandCapture args: ${args.join(" ")}`);
			});
			const runCommandSpy = vi.spyOn(managerWithInternals, "runCommand").mockResolvedValue(undefined);

			await packageManager.install(source);

			expect(runCommandSpy).toHaveBeenCalledWith("git", ["fetch", "origin", "v2"], { cwd: targetDir });
			expect(runCommandSpy).toHaveBeenCalledWith("git", ["reset", "--hard", "FETCH_HEAD^{commit}"], {
				cwd: targetDir,
			});
			expect(runCommandSpy).toHaveBeenCalledWith("git", ["clean", "-fdx"], { cwd: targetDir });
			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"],
				{
					cwd: targetDir,
				},
			);
		});

		it("should reconcile an existing git checkout to its update target when installing without a ref", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			const fetchArgs = ["fetch", "--prune", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main"];
			mkdirSync(targetDir, { recursive: true });

			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			vi.spyOn(managerWithInternals, "getLocalGitUpdateTarget").mockResolvedValue({
				ref: "origin/HEAD",
				head: "new-head",
				fetchArgs,
			});
			vi.spyOn(managerWithInternals, "runCommandCapture").mockImplementation(async (_command, args) => {
				if (args[0] === "rev-parse" && args[1] === "HEAD") {
					return "old-head";
				}
				if (args[0] === "rev-parse" && args[1] === "origin/HEAD^{commit}") {
					return "new-head";
				}
				throw new Error(`Unexpected runCommandCapture args: ${args.join(" ")}`);
			});
			const runCommandSpy = vi.spyOn(managerWithInternals, "runCommand").mockResolvedValue(undefined);

			await packageManager.install(source);

			expect(runCommandSpy).toHaveBeenCalledWith("git", fetchArgs, { cwd: targetDir });
			expect(runCommandSpy).toHaveBeenCalledWith("git", ["reset", "--hard", "origin/HEAD^{commit}"], {
				cwd: targetDir,
			});
			expect(runCommandSpy).toHaveBeenCalledWith("git", ["clean", "-fdx"], { cwd: targetDir });
		});

		it("should prefer the package manager after a separator over the outer executable", () => {
			// Regression for #9863.
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["npm", "exec", "--", "pnpm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			expect(managerWithInternals.getPackageManagerName()).toBe("pnpm");
			expect(managerWithInternals.getGitDependencyInstallArgs()).toEqual([
				"install",
				"--prod",
				"--config.auto-install-peers=false",
				"--config.strict-peer-dependencies=false",
				"--config.strict-dep-builds=false",
			]);
		});

		it("should detect pnpm through a corepack wrapper without a separator", async () => {
			// Regression for #9863.
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["corepack", "pnpm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			const runCommandSpy = vi
				.spyOn(packageManager as any, "runCommand")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					if (command === "git" && args[0] === "clone") {
						mkdirSync(targetDir, { recursive: true });
						writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
					}
				});

			await packageManager.install(source);

			expect(runCommandSpy).toHaveBeenCalledWith(
				"corepack",
				[
					"pnpm",
					"install",
					"--prod",
					"--config.auto-install-peers=false",
					"--config.strict-peer-dependencies=false",
					"--config.strict-dep-builds=false",
				],
				{ cwd: targetDir },
			);
		});

		it("should disable peer installation for git package dependencies with bun", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["bun"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			const runCommandSpy = vi
				.spyOn(packageManager as any, "runCommand")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					if (command === "git" && args[0] === "clone") {
						mkdirSync(targetDir, { recursive: true });
						writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
					}
				});

			await packageManager.install(source);

			expect(runCommandSpy).toHaveBeenCalledWith("bun", ["install", "--omit=dev", "--omit=peer"], {
				cwd: targetDir,
			});
		});

		it("should update git package dependencies without auto-installing peers", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(tempDir, ".mehh", "git", "github.com", "user", "repo");
			mkdirSync(targetDir, { recursive: true });
			writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
			settingsManager.setProjectPackages([source]);

			vi.spyOn(packageManager as any, "runCommandCapture").mockImplementation(async (...callArgs: unknown[]) => {
				const [_command, args] = callArgs as [string, string[]];
				if (args[0] === "rev-parse" && args[1] === "--abbrev-ref" && args[2] === "@{upstream}") {
					return "origin/main";
				}
				if (args[0] === "rev-parse" && (args[1] === "@{upstream}" || args[1] === "@{upstream}^{commit}")) {
					return "remote-head";
				}
				if (args[0] === "rev-parse" && args[1] === "HEAD") {
					return "local-head";
				}
				throw new Error(`Unexpected runCommandCapture args: ${args.join(" ")}`);
			});
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			await packageManager.update(source);

			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"],
				{
					cwd: targetDir,
				},
			);
		});

		it("should repair missing git package dependencies when the checkout is already current", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			const fetchArgs = ["fetch", "--prune", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main"];
			mkdirSync(targetDir, { recursive: true });
			writeFileSync(
				join(targetDir, "package.json"),
				JSON.stringify({ name: "repo", version: "1.0.0", dependencies: { dependency: "1.0.0" } }),
			);
			settingsManager.setPackages([source]);

			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			vi.spyOn(managerWithInternals, "getLocalGitUpdateTarget").mockResolvedValue({
				ref: "@{upstream}",
				head: "current-head",
				fetchArgs,
			});
			vi.spyOn(managerWithInternals, "runCommandCapture").mockResolvedValue("current-head");
			const runCommandSpy = vi.spyOn(managerWithInternals, "runCommand").mockResolvedValue(undefined);

			await packageManager.update(source);

			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"],
				{
					cwd: targetDir,
				},
			);
			expect(runCommandSpy).not.toHaveBeenCalledWith("git", ["clean", "-fdx"], { cwd: targetDir });
		});

		// https://github.com/mehh-code/mehh-code/issues/11597
		it("should install git package dependencies when cleaning fails", async () => {
			const source = "git:github.com/user/repo";
			const targetDir = join(agentDir, "git", "github.com", "user", "repo");
			const fetchArgs = ["fetch", "--prune", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main"];
			mkdirSync(targetDir, { recursive: true });
			writeFileSync(
				join(targetDir, "package.json"),
				JSON.stringify({ name: "repo", version: "1.0.0", dependencies: { dependency: "1.0.0" } }),
			);
			settingsManager.setPackages([source]);

			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			vi.spyOn(managerWithInternals, "getLocalGitUpdateTarget").mockResolvedValue({
				ref: "@{upstream}",
				head: "new-head",
				fetchArgs,
			});
			vi.spyOn(managerWithInternals, "runCommandCapture").mockImplementation(async (_command, args) =>
				args[1] === "HEAD" ? "old-head" : "new-head",
			);
			const runCommandSpy = vi
				.spyOn(managerWithInternals, "runCommand")
				.mockImplementation(async (_command, args) => {
					if (args[0] === "clean") throw new Error("simulated clean failure");
				});

			await packageManager.update(source);

			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				["install", "--omit=dev", "--legacy-peer-deps", "--no-audit", "--no-fund"],
				{
					cwd: targetDir,
				},
			);
		});

		it("should disable peer installation through wrapped pnpm when updating git dependencies", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["mise", "exec", "node@20", "--", "pnpm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const source = "git:github.com/user/repo";
			const targetDir = join(tempDir, ".mehh", "git", "github.com", "user", "repo");
			mkdirSync(targetDir, { recursive: true });
			writeFileSync(join(targetDir, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
			settingsManager.setProjectPackages([source]);

			vi.spyOn(packageManager as any, "runCommandCapture").mockImplementation(async (...callArgs: unknown[]) => {
				const [_command, args] = callArgs as [string, string[]];
				if (args[0] === "rev-parse" && args[1] === "--abbrev-ref" && args[2] === "@{upstream}") {
					return "origin/main";
				}
				if (args[0] === "rev-parse" && (args[1] === "@{upstream}" || args[1] === "@{upstream}^{commit}")) {
					return "remote-head";
				}
				if (args[0] === "rev-parse" && args[1] === "HEAD") {
					return "local-head";
				}
				throw new Error(`Unexpected runCommandCapture args: ${args.join(" ")}`);
			});
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			await packageManager.update(source);

			expect(runCommandSpy).toHaveBeenCalledWith(
				"mise",
				[
					"exec",
					"node@20",
					"--",
					"pnpm",
					"install",
					"--prod",
					"--config.auto-install-peers=false",
					"--config.strict-peer-dependencies=false",
					"--config.strict-dep-builds=false",
				],
				{ cwd: targetDir },
			);
		});

		it("should use npmCommand argv for npm root lookup and invalidate cached root when npmCommand changes", () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["mise", "exec", "node@20", "--", "npm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const root20 = join(tempDir, "node20", "lib", "node_modules");
			const root22 = join(tempDir, "node22", "lib", "node_modules");
			mkdirSync(join(root20, "@scope", "pkg"), { recursive: true });

			const runCommandSyncSpy = vi
				.spyOn(packageManager as any, "runCommandSync")
				.mockImplementation((...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					if (command !== "mise") {
						throw new Error(`unexpected command ${command}`);
					}
					if (args[1] === "node@20") {
						return root20;
					}
					if (args[1] === "node@22") {
						return root22;
					}
					throw new Error(`unexpected args ${args.join(" ")}`);
				});

			expect(packageManager.getInstalledPath("npm:@scope/pkg", "user")).toBe(join(root20, "@scope", "pkg"));
			expect(runCommandSyncSpy).toHaveBeenNthCalledWith(1, "mise", ["exec", "node@20", "--", "npm", "root", "-g"]);

			settingsManager.setNpmCommand(["mise", "exec", "node@22", "--", "npm"]);

			expect(packageManager.getInstalledPath("npm:@scope/pkg", "user")).toBeUndefined();
			expect(runCommandSyncSpy).toHaveBeenNthCalledWith(2, "mise", ["exec", "node@22", "--", "npm", "root", "-g"]);
		});

		it("should install user npm packages into the mehh-managed npm root", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["pnpm"],
				packages: ["npm:pnpm-pkg"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const packagePath = join(agentDir, "npm", "node_modules", "pnpm-pkg");
			vi.spyOn(packageManager as any, "runCommandSync").mockImplementation(() => {
				throw new Error("legacy lookup unavailable");
			});
			const runCommandSpy = vi
				.spyOn(packageManager as any, "runCommand")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					expect(command).toBe("pnpm");
					expect(args).toEqual([
						"install",
						"pnpm-pkg",
						"--prefix",
						realpathSync(join(agentDir, "npm")),
						"--config.auto-install-peers=false",
						"--config.strict-peer-dependencies=false",
						"--config.strict-dep-builds=false",
					]);
					mkdirSync(join(packagePath, "prompts"), { recursive: true });
					writeFileSync(join(packagePath, "package.json"), JSON.stringify({ name: "pnpm-pkg", version: "1.0.0" }));
					writeFileSync(join(packagePath, "prompts", "index.md"), "Review");
				});

			const first = await packageManager.resolve();
			const second = await packageManager.resolve();

			expect(first.prompts.some((r) => r.path === join(packagePath, "prompts", "index.md") && r.enabled)).toBe(true);
			expect(second.prompts.some((r) => r.path === join(packagePath, "prompts", "index.md") && r.enabled)).toBe(
				true,
			);
			expect(runCommandSpy).toHaveBeenCalledTimes(1);
			expect(packageManager.getInstalledPath("npm:pnpm-pkg", "user")).toBe(packagePath);
		});

		it("should load legacy pnpm global package paths from pnpm list output", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["pnpm"],
				packages: ["npm:pnpm-pkg"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const pnpmRoot = join(tempDir, "pnpm", "global", "v11");
			const packagePath = join(pnpmRoot, "20-hash", "node_modules", "pnpm-pkg");
			mkdirSync(join(packagePath, "prompts"), { recursive: true });
			writeFileSync(join(packagePath, "package.json"), JSON.stringify({ name: "pnpm-pkg", version: "1.0.0" }));
			writeFileSync(join(packagePath, "prompts", "index.md"), "Review");

			vi.spyOn(packageManager as any, "runCommandSync").mockImplementation((...callArgs: unknown[]) => {
				const [command, args] = callArgs as [string, string[]];
				if (command !== "pnpm") {
					throw new Error(`unexpected command ${command}`);
				}
				if (args.join(" ") === "list -g --depth 0 --json") {
					return JSON.stringify([
						{
							path: pnpmRoot,
							dependencies: { "pnpm-pkg": { version: "1.0.0", path: packagePath } },
						},
					]);
				}
				throw new Error(`unexpected args ${args.join(" ")}`);
			});
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			const result = await packageManager.resolve();

			expect(result.prompts.some((r) => r.path === join(packagePath, "prompts", "index.md") && r.enabled)).toBe(
				true,
			);
			expect(runCommandSpy).not.toHaveBeenCalled();
			expect(packageManager.getInstalledPath("npm:pnpm-pkg", "user")).toBe(packagePath);
		});

		it("should resolve wrapped pnpm global package paths from pnpm list output", () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["mise", "exec", "node@20", "--", "pnpm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const pnpmRoot = join(tempDir, "pnpm", "global", "v11");
			const packagePath = join(pnpmRoot, "20-hash", "node_modules", "pnpm-pkg");
			mkdirSync(packagePath, { recursive: true });

			vi.spyOn(packageManager as any, "runCommandSync").mockImplementation((...callArgs: unknown[]) => {
				const [command, args] = callArgs as [string, string[]];
				expect(command).toBe("mise");
				if (args.join(" ") === "exec node@20 -- pnpm list -g --depth 0 --json") {
					return JSON.stringify([{ path: pnpmRoot, dependencies: { "pnpm-pkg": { path: packagePath } } }]);
				}
				throw new Error(`unexpected args ${args.join(" ")}`);
			});

			expect(packageManager.getInstalledPath("npm:pnpm-pkg", "user")).toBe(packagePath);
		});

		it("should ignore malformed legacy pnpm global package lists", () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["pnpm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			vi.spyOn(packageManager as any, "runCommandSync").mockReturnValue("not json");

			expect(packageManager.getInstalledPath("npm:pnpm-pkg", "user")).toBeUndefined();
		});
	});

	describe("source parsing", () => {
		it("should emit progress events on install attempt", async () => {
			const events: ProgressEvent[] = [];
			packageManager.setProgressCallback((event) => events.push(event));
			vi.spyOn(packageManager as unknown as PackageManagerInternals, "runCommand").mockRejectedValue(
				new Error("simulated npm install failure"),
			);

			await expect(packageManager.install("npm:nonexistent-package@1.0.0")).rejects.toThrow(
				"simulated npm install failure",
			);

			expect(events.some((e) => e.type === "start" && e.action === "install")).toBe(true);
			expect(events.some((e) => e.type === "error")).toBe(true);
		});

		it("should recognize github URLs without git: prefix", async () => {
			const events: ProgressEvent[] = [];
			packageManager.setProgressCallback((event) => events.push(event));
			const runCommand = vi
				.spyOn(packageManager as unknown as PackageManagerInternals, "runCommand")
				.mockRejectedValue(new Error("simulated git clone failure"));
			const source = "https://github.com/nonexistent/repo";

			await expect(packageManager.install(source)).rejects.toThrow("simulated git clone failure");

			expect(runCommand).toHaveBeenCalledWith("git", ["clone", source, expect.any(String)]);
			expect(events.some((e) => e.type === "start" && e.action === "install")).toBe(true);
		});

		it("should parse package source types from docs examples", () => {
			const parseNpm = (source: string) => {
				const parsed = (packageManager as any).parseSource(source);
				if (parsed.type !== "npm") {
					throw new Error(`Expected npm source: ${source}`);
				}
				return parsed;
			};

			expect(parseNpm("npm:@scope/pkg@1.2.3").pinned).toBe(true);
			expect(parseNpm("npm:@scope/pkg@^1.2.3").pinned).toBe(false);
			expect(parseNpm("npm:pkg").pinned).toBe(false);

			expect((packageManager as any).parseSource("git:github.com/user/repo@v1").type).toBe("git");
			expect((packageManager as any).parseSource("https://github.com/user/repo@v1").type).toBe("git");
			expect((packageManager as any).parseSource("git:git@github.com:user/repo@v1").type).toBe("git");
			expect((packageManager as any).parseSource("ssh://git@github.com/user/repo@v1").type).toBe("git");

			expect((packageManager as any).parseSource("/absolute/path/to/package").type).toBe("local");
			expect((packageManager as any).parseSource("./relative/path/to/package").type).toBe("local");
			expect((packageManager as any).parseSource("../relative/path/to/package").type).toBe("local");
		});

		it("should never parse dot-relative paths as git", () => {
			const dotSlash = (packageManager as any).parseSource("./packages/agent-timers");
			expect(dotSlash.type).toBe("local");
			expect(dotSlash.path).toBe("./packages/agent-timers");

			const dotDotSlash = (packageManager as any).parseSource("../packages/agent-timers");
			expect(dotDotSlash.type).toBe("local");
			expect(dotDotSlash.path).toBe("../packages/agent-timers");
		});
	});

	describe("git install paths", () => {
		it("should reject paths outside git install roots", () => {
			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			const traversalSource = {
				type: "git" as const,
				repo: "git@evil.example:../../victim/repo",
				host: "evil.example",
				path: "../../victim/repo",
				pinned: false,
			};

			for (const scope of ["user", "project"] as const) {
				expect(() => managerWithInternals.getGitInstallPath(traversalSource, scope)).toThrow(
					"outside package install root",
				);
			}
		});
	});

	describe("settings source normalization", () => {
		it("should store global local packages relative to agent settings base", () => {
			const pkgDir = join(tempDir, "packages", "local-global-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "index.md"), "Review");

			const added = packageManager.addSourceToSettings("./packages/local-global-pkg");
			expect(added).toBe(true);

			const settings = settingsManager.getGlobalSettings();
			const rel = relative(agentDir, pkgDir);
			const expected = rel.startsWith(".") ? rel : `./${rel}`;
			expect(settings.packages?.[0]).toBe(expected);
		});

		it("should store project local packages relative to .mehh settings base", () => {
			const projectPkgDir = join(tempDir, "project-local-pkg");
			mkdirSync(join(projectPkgDir, "prompts"), { recursive: true });
			writeFileSync(join(projectPkgDir, "prompts", "index.md"), "Review");

			const added = packageManager.addSourceToSettings("./project-local-pkg", { local: true });
			expect(added).toBe(true);

			const settings = settingsManager.getProjectSettings();
			const rel = relative(join(tempDir, ".mehh"), projectPkgDir);
			const expected = rel.startsWith(".") ? rel : `./${rel}`;
			expect(settings.packages?.[0]).toBe(expected);
		});

		it("should remove local package entries using equivalent path forms", () => {
			const pkgDir = join(tempDir, "remove-local-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "index.md"), "Review");

			packageManager.addSourceToSettings("./remove-local-pkg");
			const removed = packageManager.removeSourceFromSettings(`${pkgDir}/`);
			expect(removed).toBe(true);
			expect(settingsManager.getGlobalSettings().packages ?? []).toHaveLength(0);
		});

		it("should return false when adding the same git source with the same ref", () => {
			const first = packageManager.addSourceToSettings("git:github.com/user/repo@v1");
			expect(first).toBe(true);

			const second = packageManager.addSourceToSettings("git:github.com/user/repo@v1");
			expect(second).toBe(false);
			expect(settingsManager.getGlobalSettings().packages).toEqual(["git:github.com/user/repo@v1"]);
		});

		it("should update the ref when adding the same git source with a different ref", () => {
			packageManager.addSourceToSettings("git:github.com/user/repo@v1");

			const updated = packageManager.addSourceToSettings("git:github.com/user/repo@v2");
			expect(updated).toBe(true);
			expect(settingsManager.getGlobalSettings().packages).toEqual(["git:github.com/user/repo@v2"]);
		});

		it("should preserve package filters when replacing a package source ref", () => {
			settingsManager.setPackages([
				{
					source: "git:github.com/user/repo@v1",
					skills: [],
					prompts: ["prompts/review.md"],
					themes: ["themes/dark.json"],
				},
			]);

			const updated = packageManager.addSourceToSettings("git:github.com/user/repo@v2");
			expect(updated).toBe(true);
			expect(settingsManager.getGlobalSettings().packages).toEqual([
				{
					source: "git:github.com/user/repo@v2",
					skills: [],
					prompts: ["prompts/review.md"],
					themes: ["themes/dark.json"],
				},
			]);
		});
	});

	describe("HTTPS git URL parsing (old behavior)", () => {
		it("should parse HTTPS GitHub URLs correctly", async () => {
			const parsed = (packageManager as any).parseSource("https://github.com/user/repo");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("github.com");
			expect(parsed.path).toBe("user/repo");
			expect(parsed.pinned).toBe(false);
		});

		it("should parse HTTPS URLs with git: prefix", async () => {
			const parsed = (packageManager as any).parseSource("git:https://github.com/user/repo");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("github.com");
			expect(parsed.path).toBe("user/repo");
		});

		it("should parse HTTPS URLs with ref", async () => {
			const parsed = (packageManager as any).parseSource("https://github.com/user/repo@v1.2.3");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("github.com");
			expect(parsed.path).toBe("user/repo");
			expect(parsed.ref).toBe("v1.2.3");
			expect(parsed.pinned).toBe(true);
		});

		it("should parse host/path shorthand only with git: prefix", async () => {
			const parsed = (packageManager as any).parseSource("git:github.com/user/repo");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("github.com");
			expect(parsed.path).toBe("user/repo");
		});

		it("should treat host/path shorthand as local without git: prefix", async () => {
			const parsed = (packageManager as any).parseSource("github.com/user/repo");
			expect(parsed.type).toBe("local");
		});

		it("should parse HTTPS URLs with .git suffix", async () => {
			const parsed = (packageManager as any).parseSource("https://github.com/user/repo.git");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("github.com");
			expect(parsed.path).toBe("user/repo");
		});

		it("should parse GitLab HTTPS URLs", async () => {
			const parsed = (packageManager as any).parseSource("https://gitlab.com/user/repo");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("gitlab.com");
			expect(parsed.path).toBe("user/repo");
		});

		it("should parse Bitbucket HTTPS URLs", async () => {
			const parsed = (packageManager as any).parseSource("https://bitbucket.org/user/repo");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("bitbucket.org");
			expect(parsed.path).toBe("user/repo");
		});

		it("should parse Codeberg HTTPS URLs", async () => {
			const parsed = (packageManager as any).parseSource("https://codeberg.org/user/repo");
			expect(parsed.type).toBe("git");
			expect(parsed.host).toBe("codeberg.org");
			expect(parsed.path).toBe("user/repo");
		});

		it("should generate correct package identity for protocol and git:-prefixed URLs", async () => {
			const identity1 = (packageManager as any).getPackageIdentity("https://github.com/user/repo");
			const identity2 = (packageManager as any).getPackageIdentity("https://github.com/user/repo@v1.0.0");
			const identity3 = (packageManager as any).getPackageIdentity("git:github.com/user/repo");
			const identity4 = (packageManager as any).getPackageIdentity("https://github.com/user/repo.git");

			// All should have the same identity (normalized)
			expect(identity1).toBe("git:github.com/user/repo");
			expect(identity2).toBe("git:github.com/user/repo");
			expect(identity3).toBe("git:github.com/user/repo");
			expect(identity4).toBe("git:github.com/user/repo");
		});

		it("should deduplicate git URLs with different supported formats", async () => {
			const pkgDir = join(tempDir, "https-dedup-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "test.md"), "Test");

			// Mock the package as if it were cloned from different URL formats
			// In reality, these would all point to the same local dir after install
			settingsManager.setPackages([
				"https://github.com/user/repo",
				"git:github.com/user/repo",
				"https://github.com/user/repo.git",
			]);

			// Since these URLs don't actually exist and we can't clone them,
			// we verify they produce the same identity
			const id1 = (packageManager as any).getPackageIdentity("https://github.com/user/repo");
			const id2 = (packageManager as any).getPackageIdentity("git:github.com/user/repo");
			const id3 = (packageManager as any).getPackageIdentity("https://github.com/user/repo.git");

			expect(id1).toBe(id2);
			expect(id2).toBe(id3);
		});

		it("should handle HTTPS URLs with refs in resolve", async () => {
			// This tests that the ref is properly extracted and stored
			const parsed = (packageManager as any).parseSource("https://github.com/user/repo@main");
			expect(parsed.ref).toBe("main");
			expect(parsed.pinned).toBe(true);

			const parsed2 = (packageManager as any).parseSource("https://github.com/user/repo@feature/branch");
			expect(parsed2.ref).toBe("feature/branch");
		});
	});

	describe("pattern filtering in top-level arrays", () => {
		// #10765
		it("should expand ~ in exclusion and exact patterns", async () => {
			const previousHome = process.env.HOME;
			process.env.HOME = tempDir;

			try {
				const skillsDir = join(tempDir, "extra", "skills");
				for (const name of ["keep", "glob-excluded", "exact-excluded"]) {
					mkdirSync(join(skillsDir, name), { recursive: true });
					writeFileSync(join(skillsDir, name, "SKILL.md"), `---\nname: ${name}\ndescription: ${name}\n---\n`);
				}

				settingsManager.setSkillPaths([
					"~/extra/skills",
					"!~/extra/skills/glob-*",
					"-~/extra/skills/exact-excluded",
				]);

				const result = await packageManager.resolve();
				expect(result.skills.some((r) => isEnabled(r, "keep/SKILL.md"))).toBe(true);
				expect(result.skills.some((r) => isDisabled(r, "glob-excluded/SKILL.md"))).toBe(true);
				expect(result.skills.some((r) => isDisabled(r, "exact-excluded/SKILL.md"))).toBe(true);
			} finally {
				if (previousHome === undefined) {
					delete process.env.HOME;
				} else {
					process.env.HOME = previousHome;
				}
			}
		});

		it("should filter themes with glob patterns", async () => {
			const themesDir = join(agentDir, "themes");
			mkdirSync(themesDir, { recursive: true });
			writeFileSync(join(themesDir, "dark.json"), "{}");
			writeFileSync(join(themesDir, "light.json"), "{}");
			writeFileSync(join(themesDir, "funky.json"), "{}");

			settingsManager.setThemePaths(["themes", "!funky.json"]);

			const result = await packageManager.resolve();
			expect(result.themes.some((r) => isEnabled(r, "dark.json"))).toBe(true);
			expect(result.themes.some((r) => isEnabled(r, "light.json"))).toBe(true);
			expect(result.themes.some((r) => isDisabled(r, "funky.json"))).toBe(true);
		});

		it("should filter prompts with exclusion pattern", async () => {
			const promptsDir = join(agentDir, "prompts");
			mkdirSync(promptsDir, { recursive: true });
			writeFileSync(join(promptsDir, "review.md"), "Review code");
			writeFileSync(join(promptsDir, "explain.md"), "Explain code");

			settingsManager.setPromptTemplatePaths(["prompts", "!explain.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isEnabled(r, "review.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "explain.md"))).toBe(true);
		});

		it("should filter skills with exclusion pattern", async () => {
			const skillsDir = join(agentDir, "skills");
			mkdirSync(join(skillsDir, "good-skill"), { recursive: true });
			mkdirSync(join(skillsDir, "bad-skill"), { recursive: true });
			writeFileSync(
				join(skillsDir, "good-skill", "SKILL.md"),
				"---\nname: good-skill\ndescription: Good\n---\nContent",
			);
			writeFileSync(
				join(skillsDir, "bad-skill", "SKILL.md"),
				"---\nname: bad-skill\ndescription: Bad\n---\nContent",
			);

			settingsManager.setSkillPaths(["skills", "!**/bad-skill"]);

			const result = await packageManager.resolve();
			expect(result.skills.some((r) => isEnabled(r, "good-skill", "includes"))).toBe(true);
			expect(result.skills.some((r) => isDisabled(r, "bad-skill", "includes"))).toBe(true);
		});
	});

	describe("pattern filtering in mehh manifest", () => {
		it("should support glob patterns in manifest skills", async () => {
			const pkgDir = join(tempDir, "skill-manifest-pkg");
			mkdirSync(join(pkgDir, "skills/good-skill"), { recursive: true });
			mkdirSync(join(pkgDir, "skills/bad-skill"), { recursive: true });
			writeFileSync(
				join(pkgDir, "skills/good-skill", "SKILL.md"),
				"---\nname: good-skill\ndescription: Good\n---\nContent",
			);
			writeFileSync(
				join(pkgDir, "skills/bad-skill", "SKILL.md"),
				"---\nname: bad-skill\ndescription: Bad\n---\nContent",
			);
			writeFileSync(
				join(pkgDir, "package.json"),
				JSON.stringify({
					name: "skill-manifest-pkg",
					mehh: {
						skills: ["skills", "!**/bad-skill"],
					},
				}),
			);

			settingsManager.setPackages([pkgDir]);
			const result = await packageManager.resolve();
			expect(result.skills.some((r) => isEnabled(r, "good-skill", "includes"))).toBe(true);
			expect(result.skills.some((r) => r.path.includes("bad-skill"))).toBe(false);
		});

		it("should expand positive glob manifest entries before collecting skills", async () => {
			const pkgDir = join(tempDir, "skill-manifest-glob-pkg");
			mkdirSync(join(pkgDir, "plugins/pdf-to-markdown/skills/pdf-to-markdown"), { recursive: true });
			mkdirSync(join(pkgDir, "plugins/nutrient-dws/skills/document-processor-api"), { recursive: true });
			writeFileSync(
				join(pkgDir, "plugins/pdf-to-markdown/skills/pdf-to-markdown", "SKILL.md"),
				"---\nname: pdf-to-markdown\ndescription: PDF to Markdown\n---\nContent",
			);
			writeFileSync(
				join(pkgDir, "plugins/nutrient-dws/skills/document-processor-api", "SKILL.md"),
				"---\nname: document-processor-api\ndescription: DWS\n---\nContent",
			);
			writeFileSync(
				join(pkgDir, "package.json"),
				JSON.stringify({
					name: "skill-manifest-glob-pkg",
					mehh: {
						skills: ["./plugins/*/skills"],
					},
				}),
			);

			settingsManager.setPackages([pkgDir]);
			const result = await packageManager.resolve();
			expect(result.skills.some((r) => isEnabled(r, "pdf-to-markdown", "includes"))).toBe(true);
			expect(result.skills.some((r) => isEnabled(r, "document-processor-api", "includes"))).toBe(true);
		});

		it("should use exact manifest entries for symlinked skill directories", async () => {
			const pkgDir = join(tempDir, "manifest-glob-semantics-pkg");
			const linkedPluginSource = join(pkgDir, "linked-plugin-source");
			mkdirSync(join(pkgDir, "plugins", "local", "skills", "local-skill"), { recursive: true });
			mkdirSync(join(linkedPluginSource, "skills", "linked-skill"), { recursive: true });
			writeFileSync(
				join(pkgDir, "plugins", "local", "skills", "local-skill", "SKILL.md"),
				"---\nname: local-skill\ndescription: Local\n---\n",
			);
			writeFileSync(
				join(linkedPluginSource, "skills", "linked-skill", "SKILL.md"),
				"---\nname: linked-skill\ndescription: Linked\n---\n",
			);
			symlinkSync(
				linkedPluginSource,
				join(pkgDir, "plugins", "linked"),
				process.platform === "win32" ? "junction" : "dir",
			);
			writeFileSync(
				join(pkgDir, "package.json"),
				JSON.stringify({
					name: "manifest-glob-semantics-pkg",
					mehh: {
						skills: ["./plugins/*/skills", "./plugins/linked/skills"],
					},
				}),
			);

			settingsManager.setPackages([pkgDir]);
			const result = await packageManager.resolve();
			expect(result.skills.some((resource) => pathEndsWith(resource.path, "local-skill/SKILL.md"))).toBe(true);
			expect(result.skills.some((resource) => pathEndsWith(resource.path, "linked-skill/SKILL.md"))).toBe(true);
		});
	});

	describe("pattern filtering in package filters", () => {
		it("should apply user filters on top of manifest filters (not replace)", async () => {
			// Manifest excludes baz.md, user excludes bar.md
			// Result should exclude BOTH
			const pkgDir = join(tempDir, "layered-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "foo.md"), "Foo");
			writeFileSync(join(pkgDir, "prompts", "bar.md"), "Bar");
			writeFileSync(join(pkgDir, "prompts", "baz.md"), "Baz");
			writeFileSync(
				join(pkgDir, "package.json"),
				JSON.stringify({
					name: "layered-pkg",
					mehh: {
						prompts: ["prompts", "!**/baz.md"],
					},
				}),
			);

			// User filter adds exclusion for bar.md
			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: ["!**/bar.md"],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			// foo.md should be included (not excluded by anyone)
			expect(result.prompts.some((r) => isEnabled(r, "foo.md"))).toBe(true);
			// bar.md should be excluded (by user)
			expect(result.prompts.some((r) => isDisabled(r, "bar.md"))).toBe(true);
			// baz.md should be excluded (by manifest)
			expect(result.prompts.some((r) => pathEndsWith(r.path, "baz.md"))).toBe(false);
		});

		it("should exclude prompts from package with ! pattern", async () => {
			const pkgDir = join(tempDir, "pattern-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "foo.md"), "Foo");
			writeFileSync(join(pkgDir, "prompts", "bar.md"), "Bar");
			writeFileSync(join(pkgDir, "prompts", "baz.md"), "Baz");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: ["!**/baz.md"],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isEnabled(r, "foo.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "bar.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "baz.md"))).toBe(true);
		});

		it("should filter themes from package", async () => {
			const pkgDir = join(tempDir, "theme-pkg");
			mkdirSync(join(pkgDir, "themes"), { recursive: true });
			writeFileSync(join(pkgDir, "themes", "nice.json"), "{}");
			writeFileSync(join(pkgDir, "themes", "ugly.json"), "{}");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: [],
					themes: ["!ugly.json"],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.themes.some((r) => isEnabled(r, "nice.json"))).toBe(true);
			expect(result.themes.some((r) => isDisabled(r, "ugly.json"))).toBe(true);
		});

		it("should combine include and exclude patterns", async () => {
			const pkgDir = join(tempDir, "combo-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "alpha.md"), "Alpha");
			writeFileSync(join(pkgDir, "prompts", "beta.md"), "Beta");
			writeFileSync(join(pkgDir, "prompts", "gamma.md"), "Gamma");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: ["**/alpha.md", "**/beta.md", "!**/beta.md"],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isEnabled(r, "alpha.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "beta.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "gamma.md"))).toBe(true);
		});

		it("should work with direct paths (no patterns)", async () => {
			const pkgDir = join(tempDir, "direct-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "one.md"), "One");
			writeFileSync(join(pkgDir, "prompts", "two.md"), "Two");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: ["prompts/one.md"],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isEnabled(r, "one.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "two.md"))).toBe(true);
		});

		it("should resolve autoload-disabled project package entries as deltas over global packages", async () => {
			const pkgDir = join(agentDir, "npm", "node_modules", "mehh-tools");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "package.json"), JSON.stringify({ name: "mehh-tools", version: "1.0.0" }));
			writeFileSync(join(pkgDir, "prompts", "foo.md"), "Foo");
			writeFileSync(join(pkgDir, "prompts", "bar.md"), "Bar");
			settingsManager.setPackages(["npm:mehh-tools"]);
			settingsManager.setProjectPackages([
				{ source: "npm:mehh-tools", autoload: false, prompts: ["-prompts/foo.md"] },
			]);
			const runCommandSpy = vi
				.spyOn(packageManager as unknown as PackageManagerInternals, "runCommand")
				.mockRejectedValue(new Error("unexpected install"));

			const result = await packageManager.resolve();
			const states = Object.fromEntries(
				result.prompts.map((resource) => [
					resource.path,
					{ enabled: resource.enabled, scope: resource.metadata.scope },
				]),
			);
			expect(runCommandSpy).not.toHaveBeenCalled();
			expect(states[join(pkgDir, "prompts", "foo.md")]).toEqual({ enabled: false, scope: "project" });
			expect(states[join(pkgDir, "prompts", "bar.md")]).toEqual({ enabled: true, scope: "user" });
		});

		it("should resolve autoload-disabled package entries as positive-only without a global package", async () => {
			vi.stubEnv("HOME", tempDir);
			const pkgDir = join(tempDir, "positive-only-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			mkdirSync(join(pkgDir, "skills", "foo"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "foo.md"), "Foo");
			writeFileSync(join(pkgDir, "prompts", "bar.md"), "Bar");
			writeFileSync(join(pkgDir, "skills", "foo", "SKILL.md"), "# Foo\n");
			settingsManager.setProjectPackages([
				{ source: relative(join(tempDir, ".mehh"), pkgDir), autoload: false, prompts: ["+prompts/foo.md"] },
			]);

			const result = await packageManager.resolve();

			expect(result.prompts.map((resource) => resource.path)).toEqual([join(pkgDir, "prompts", "foo.md")]);
			expect(result.skills).toEqual([]);
		});
	});

	describe("force-include patterns", () => {
		it("should force-include prompts with + pattern after exclusion", async () => {
			const promptsDir = join(agentDir, "prompts");
			mkdirSync(promptsDir, { recursive: true });
			writeFileSync(join(promptsDir, "keep.md"), "Keep");
			writeFileSync(join(promptsDir, "excluded.md"), "Excluded");
			writeFileSync(join(promptsDir, "force-back.md"), "Force back");

			// Exclude all, then force-include one back
			settingsManager.setPromptTemplatePaths(["prompts", "!prompts/*.md", "+prompts/force-back.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isDisabled(r, "keep.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "excluded.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "force-back.md"))).toBe(true);
		});

		it("should force-include overrides exclude in package filters", async () => {
			const pkgDir = join(tempDir, "force-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "alpha.md"), "Alpha");
			writeFileSync(join(pkgDir, "prompts", "beta.md"), "Beta");
			writeFileSync(join(pkgDir, "prompts", "gamma.md"), "Gamma");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: ["!**/*.md", "+prompts/beta.md"],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isDisabled(r, "alpha.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "beta.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "gamma.md"))).toBe(true);
		});

		it("should force-include multiple resources", async () => {
			const pkgDir = join(tempDir, "multi-force-pkg");
			mkdirSync(join(pkgDir, "skills/skill-a"), { recursive: true });
			mkdirSync(join(pkgDir, "skills/skill-b"), { recursive: true });
			mkdirSync(join(pkgDir, "skills/skill-c"), { recursive: true });
			writeFileSync(join(pkgDir, "skills/skill-a", "SKILL.md"), "---\nname: skill-a\ndescription: A\n---\nContent");
			writeFileSync(join(pkgDir, "skills/skill-b", "SKILL.md"), "---\nname: skill-b\ndescription: B\n---\nContent");
			writeFileSync(join(pkgDir, "skills/skill-c", "SKILL.md"), "---\nname: skill-c\ndescription: C\n---\nContent");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: ["!**/*", "+skills/skill-a", "+skills/skill-c"],
					prompts: [],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.skills.some((r) => isEnabled(r, "skill-a", "includes"))).toBe(true);
			expect(result.skills.some((r) => isDisabled(r, "skill-b", "includes"))).toBe(true);
			expect(result.skills.some((r) => isEnabled(r, "skill-c", "includes"))).toBe(true);
		});

		it("should force-include after specific exclusion", async () => {
			const promptsDir = join(agentDir, "prompts");
			mkdirSync(promptsDir, { recursive: true });
			writeFileSync(join(promptsDir, "a.md"), "A");
			writeFileSync(join(promptsDir, "b.md"), "B");

			// Specifically exclude b.md, then force it back
			settingsManager.setPromptTemplatePaths(["prompts", "!prompts/b.md", "+prompts/b.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isEnabled(r, "a.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "b.md"))).toBe(true);
		});

		it("should handle force-include in manifest patterns", async () => {
			const pkgDir = join(tempDir, "manifest-force-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "one.md"), "One");
			writeFileSync(join(pkgDir, "prompts", "two.md"), "Two");
			writeFileSync(join(pkgDir, "prompts", "three.md"), "Three");
			writeFileSync(
				join(pkgDir, "package.json"),
				JSON.stringify({
					name: "manifest-force-pkg",
					mehh: {
						prompts: ["prompts", "!**/two.md", "+prompts/two.md"],
					},
				}),
			);

			settingsManager.setPackages([pkgDir]);
			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isEnabled(r, "one.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "two.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "three.md"))).toBe(true);
		});

		it("should force-include themes", async () => {
			const themesDir = join(agentDir, "themes");
			mkdirSync(themesDir, { recursive: true });
			writeFileSync(join(themesDir, "dark.json"), "{}");
			writeFileSync(join(themesDir, "light.json"), "{}");
			writeFileSync(join(themesDir, "special.json"), "{}");

			settingsManager.setThemePaths(["themes", "!themes/*.json", "+themes/special.json"]);

			const result = await packageManager.resolve();
			expect(result.themes.some((r) => isDisabled(r, "dark.json"))).toBe(true);
			expect(result.themes.some((r) => isDisabled(r, "light.json"))).toBe(true);
			expect(result.themes.some((r) => isEnabled(r, "special.json"))).toBe(true);
		});

		it("should force-include prompts", async () => {
			const promptsDir = join(agentDir, "prompts");
			mkdirSync(promptsDir, { recursive: true });
			writeFileSync(join(promptsDir, "review.md"), "Review");
			writeFileSync(join(promptsDir, "explain.md"), "Explain");
			writeFileSync(join(promptsDir, "debug.md"), "Debug");

			settingsManager.setPromptTemplatePaths(["prompts", "!prompts/*.md", "+prompts/debug.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isDisabled(r, "review.md"))).toBe(true);
			expect(result.prompts.some((r) => isDisabled(r, "explain.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "debug.md"))).toBe(true);
		});
	});

	describe("force-exclude patterns", () => {
		it("should force-exclude top-level resources", async () => {
			const promptsDir = join(agentDir, "prompts");
			mkdirSync(promptsDir, { recursive: true });
			writeFileSync(join(promptsDir, "alpha.md"), "Alpha");
			writeFileSync(join(promptsDir, "beta.md"), "Beta");

			settingsManager.setPromptTemplatePaths(["prompts", "+prompts/alpha.md", "-prompts/alpha.md"]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isDisabled(r, "alpha.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "beta.md"))).toBe(true);
		});

		it("should force-exclude in package filters", async () => {
			const pkgDir = join(tempDir, "force-exclude-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "alpha.md"), "Alpha");
			writeFileSync(join(pkgDir, "prompts", "beta.md"), "Beta");

			settingsManager.setPackages([
				{
					source: pkgDir,
					skills: [],
					prompts: ["prompts/*.md", "+prompts/alpha.md", "-prompts/alpha.md"],
					themes: [],
				},
			]);

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => isDisabled(r, "alpha.md"))).toBe(true);
			expect(result.prompts.some((r) => isEnabled(r, "beta.md"))).toBe(true);
		});
	});

	describe("package deduplication", () => {
		it("should dedupe same local package in global and project (project wins)", async () => {
			const pkgDir = join(tempDir, "shared-pkg");
			mkdirSync(join(pkgDir, "prompts"), { recursive: true });
			writeFileSync(join(pkgDir, "prompts", "shared.md"), "Shared");

			// Same package in both global and project
			settingsManager.setPackages([pkgDir]); // global
			settingsManager.setProjectPackages([pkgDir]); // project

			// Debug: verify settings are stored correctly
			const globalSettings = settingsManager.getGlobalSettings();
			const projectSettings = settingsManager.getProjectSettings();
			expect(globalSettings.packages).toEqual([pkgDir]);
			expect(projectSettings.packages).toEqual([pkgDir]);

			const result = await packageManager.resolve();
			// Should only appear once (deduped), with project scope
			const sharedPaths = result.prompts.filter((r) => r.path.includes("shared-pkg"));
			expect(sharedPaths.length).toBe(1);
			expect(sharedPaths[0].metadata.scope).toBe("project");
		});

		it("should keep both if different packages", async () => {
			const pkg1Dir = join(tempDir, "pkg1");
			const pkg2Dir = join(tempDir, "pkg2");
			mkdirSync(join(pkg1Dir, "prompts"), { recursive: true });
			mkdirSync(join(pkg2Dir, "prompts"), { recursive: true });
			writeFileSync(join(pkg1Dir, "prompts", "from-pkg1.md"), "From pkg1");
			writeFileSync(join(pkg2Dir, "prompts", "from-pkg2.md"), "From pkg2");

			settingsManager.setPackages([pkg1Dir]); // global
			settingsManager.setProjectPackages([pkg2Dir]); // project

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => r.path.includes("pkg1"))).toBe(true);
			expect(result.prompts.some((r) => r.path.includes("pkg2"))).toBe(true);
		});

		it("should dedupe SSH and HTTPS URLs for same repo", async () => {
			// Same repository, different URL formats
			const httpsUrl = "https://github.com/user/repo";
			const sshUrl = "git:git@github.com:user/repo";

			const httpsIdentity = (packageManager as any).getPackageIdentity(httpsUrl);
			const sshIdentity = (packageManager as any).getPackageIdentity(sshUrl);

			// Both should resolve to the same identity
			expect(httpsIdentity).toBe("git:github.com/user/repo");
			expect(sshIdentity).toBe("git:github.com/user/repo");
			expect(httpsIdentity).toBe(sshIdentity);
		});

		it("should dedupe SSH and HTTPS with refs", async () => {
			const httpsUrl = "https://github.com/user/repo@v1.0.0";
			const sshUrl = "git:git@github.com:user/repo@v1.0.0";

			const httpsIdentity = (packageManager as any).getPackageIdentity(httpsUrl);
			const sshIdentity = (packageManager as any).getPackageIdentity(sshUrl);

			// Identity should ignore ref (version)
			expect(httpsIdentity).toBe("git:github.com/user/repo");
			expect(sshIdentity).toBe("git:github.com/user/repo");
			expect(httpsIdentity).toBe(sshIdentity);
		});

		it("should dedupe SSH URL with ssh:// protocol and git@ format", async () => {
			const sshProtocol = "ssh://git@github.com/user/repo";
			const gitAt = "git:git@github.com:user/repo";

			const sshProtocolIdentity = (packageManager as any).getPackageIdentity(sshProtocol);
			const gitAtIdentity = (packageManager as any).getPackageIdentity(gitAt);

			// Both SSH formats should resolve to same identity
			expect(sshProtocolIdentity).toBe("git:github.com/user/repo");
			expect(gitAtIdentity).toBe("git:github.com/user/repo");
			expect(sshProtocolIdentity).toBe(gitAtIdentity);
		});

		it("should dedupe all supported URL formats for same repo", async () => {
			const urls = [
				"https://github.com/user/repo",
				"https://github.com/user/repo.git",
				"ssh://git@github.com/user/repo",
				"git:https://github.com/user/repo",
				"git:github.com/user/repo",
				"git:git@github.com:user/repo",
				"git:git@github.com:user/repo.git",
			];

			const identities = urls.map((url) => (packageManager as any).getPackageIdentity(url));

			// All should produce the same identity
			const uniqueIdentities = [...new Set(identities)];
			expect(uniqueIdentities.length).toBe(1);
			expect(uniqueIdentities[0]).toBe("git:github.com/user/repo");
		});

		it("should keep different repos separate (HTTPS vs SSH)", async () => {
			const repo1Https = "https://github.com/user/repo1";
			const repo2Ssh = "git:git@github.com:user/repo2";

			const id1 = (packageManager as any).getPackageIdentity(repo1Https);
			const id2 = (packageManager as any).getPackageIdentity(repo2Ssh);

			// Different repos should have different identities
			expect(id1).toBe("git:github.com/user/repo1");
			expect(id2).toBe("git:github.com/user/repo2");
			expect(id1).not.toBe(id2);
		});
	});

	describe("package install failures", () => {
		// #11088
		it("should skip a package source that fails to install and report it", async () => {
			settingsManager.setProjectPackages(["npm:broken-package"]);
			const installParsedSource = vi
				.spyOn(packageManager as any, "installParsedSource")
				.mockRejectedValue(new Error("npm install broken-package failed with code 1"));

			const result = await packageManager.resolve();

			expect(installParsedSource).toHaveBeenCalledTimes(1);
			expect(result.installFailures).toEqual([
				{
					source: "npm:broken-package",
					scope: "project",
					message: "npm install broken-package failed with code 1",
				},
			]);
			expect(result.extensions).toEqual([]);
		});

		it("should not report install failures for sources that are skipped", async () => {
			settingsManager.setProjectPackages(["npm:missing-package"]);
			const installParsedSource = vi.spyOn(packageManager as any, "installParsedSource");

			const result = await packageManager.resolve(async () => "skip");

			expect(installParsedSource).not.toHaveBeenCalled();
			expect(result.installFailures).toEqual([]);
		});

		it("should keep a missing source an error when the caller asks for one", async () => {
			settingsManager.setProjectPackages(["npm:missing-package"]);

			await expect(packageManager.resolve(async () => "error")).rejects.toThrow(
				"Missing source: npm:missing-package",
			);
		});
	});

	describe("offline mode and network timeouts", () => {
		it("should update npm range packages using the configured spec", async () => {
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedPath, { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "1.0.0" }));
			settingsManager.setProjectPackages(["npm:example@^1.0.0"]);

			const runCommandCaptureSpy = vi
				.spyOn(packageManager as any, "runCommandCapture")
				.mockResolvedValue('["1.0.0","1.2.0"]');
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockImplementation(async () => {
				writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "1.2.0" }));
			});

			const events: ProgressEvent[] = [];
			packageManager.setProgressCallback((event) => events.push(event));
			await packageManager.update("npm:example");

			// The result is a separate event, so each start still has exactly one complete.
			expect(events.map((event) => [event.type, event.source, event.message])).toEqual([
				["start", "npm:example@^1.0.0", "Updating npm:example@^1.0.0..."],
				["complete", "npm:example@^1.0.0", undefined],
				["result", "npm:example@^1.0.0", "npm:example@^1.0.0 (project) 1.0.0 -> 1.2.0"],
			]);

			expect(runCommandCaptureSpy).toHaveBeenCalledWith(
				"npm",
				["view", "example@^1.0.0", "version", "--json"],
				expect.objectContaining({ cwd: tempDir, timeoutMs: expect.any(Number) }),
			);
			expect(runCommandSpy).toHaveBeenCalledWith(
				"npm",
				[
					"install",
					"example@^1.0.0",
					"--prefix",
					realpathSync(join(tempDir, ".mehh", "npm")),
					"--legacy-peer-deps",
					"--no-audit",
					"--no-fund",
				],
				undefined,
			);
		});

		it("should skip project npm update when installed version matches latest", async () => {
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedPath, { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "1.3.1" }));
			settingsManager.setProjectPackages(["npm:example@^1.0.0"]);

			const runCommandCaptureSpy = vi
				.spyOn(packageManager as any, "runCommandCapture")
				.mockResolvedValue('["1.0.0","1.3.1","1.0.2"]');
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			const updated: string[] = [];
			packageManager.setProgressCallback((event) => {
				if (event.type === "result" && event.action === "update" && event.message) updated.push(event.message);
			});
			await packageManager.update("npm:example");

			expect(updated).toEqual([]);

			expect(runCommandCaptureSpy).toHaveBeenCalledWith(
				"npm",
				["view", "example@^1.0.0", "version", "--json"],
				expect.objectContaining({ cwd: tempDir, timeoutMs: expect.any(Number) }),
			);
			expect(runCommandSpy).not.toHaveBeenCalled();
		});

		it("should skip npm updates when the installed version is newer than the registry version", async () => {
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedPath, { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "2.0.0" }));
			settingsManager.setProjectPackages(["npm:example"]);

			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.9.0"');
			const runCommandSpy = vi.spyOn(packageManager as any, "runCommand").mockResolvedValue(undefined);

			await packageManager.update("npm:example");

			expect(runCommandCaptureSpy).toHaveBeenCalledWith(
				"npm",
				["view", "example", "version", "--json"],
				expect.objectContaining({ cwd: tempDir, timeoutMs: expect.any(Number) }),
			);
			expect(runCommandSpy).not.toHaveBeenCalled();
		});

		it("should migrate legacy user npm installs into the managed npm root during update", async () => {
			const legacyRoot = join(tempDir, "legacy-global", "node_modules");
			const legacyPath = join(legacyRoot, "legacy-pkg");
			const managedPath = join(agentDir, "npm", "node_modules", "legacy-pkg");
			mkdirSync(legacyPath, { recursive: true });
			writeFileSync(join(legacyPath, "package.json"), JSON.stringify({ name: "legacy-pkg", version: "1.0.0" }));
			settingsManager.setPackages(["npm:legacy-pkg"]);

			vi.spyOn(packageManager as any, "getGlobalNpmRoot").mockReturnValue(legacyRoot);
			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.0.0"');
			const runCommandSpy = vi
				.spyOn(packageManager as any, "runCommand")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					expect(command).toBe("npm");
					expect(args).toEqual([
						"install",
						"legacy-pkg@latest",
						"--prefix",
						realpathSync(join(agentDir, "npm")),
						"--legacy-peer-deps",
						"--no-audit",
						"--no-fund",
					]);
					mkdirSync(managedPath, { recursive: true });
					writeFileSync(
						join(managedPath, "package.json"),
						JSON.stringify({ name: "legacy-pkg", version: "1.0.0" }),
					);
				});

			expect(packageManager.getInstalledPath("npm:legacy-pkg", "user")).toBe(legacyPath);

			await packageManager.update("npm:legacy-pkg");

			expect(runCommandCaptureSpy).not.toHaveBeenCalled();
			expect(runCommandSpy).toHaveBeenCalledTimes(1);
			expect(packageManager.getInstalledPath("npm:legacy-pkg", "user")).toBe(managedPath);
		});

		it("should batch npm updates per scope and run git updates in parallel while skipping pinned npm and current packages", async () => {
			const userOldPath = join(agentDir, "npm", "node_modules", "user-old");
			const userCurrentPath = join(agentDir, "npm", "node_modules", "user-current");
			const userUnknownPath = join(agentDir, "npm", "node_modules", "user-unknown");
			const projectOldPath = join(tempDir, ".mehh", "npm", "node_modules", "project-old");
			const projectCurrentPath = join(tempDir, ".mehh", "npm", "node_modules", "project-current");
			const installPaths = [userOldPath, userCurrentPath, userUnknownPath, projectOldPath, projectCurrentPath];
			for (const installPath of installPaths) {
				mkdirSync(installPath, { recursive: true });
			}
			writeFileSync(join(userOldPath, "package.json"), JSON.stringify({ name: "user-old", version: "1.0.0" }));
			writeFileSync(
				join(userCurrentPath, "package.json"),
				JSON.stringify({ name: "user-current", version: "1.0.0" }),
			);
			writeFileSync(
				join(userUnknownPath, "package.json"),
				JSON.stringify({ name: "user-unknown", version: "1.0.0" }),
			);
			writeFileSync(join(projectOldPath, "package.json"), JSON.stringify({ name: "project-old", version: "1.0.0" }));
			writeFileSync(
				join(projectCurrentPath, "package.json"),
				JSON.stringify({ name: "project-current", version: "1.0.0" }),
			);

			settingsManager.setPackages([
				"npm:user-old",
				"npm:user-current",
				"npm:user-unknown",
				"npm:user-pinned@1.0.0",
				"git:github.com/example/user-repo-a",
				"git:github.com/example/user-repo-b",
				"git:github.com/example/user-repo-pinned@v1",
			]);
			settingsManager.setProjectPackages([
				"npm:project-old",
				"npm:project-current",
				"npm:project-missing",
				"git:github.com/example/project-repo-a",
			]);

			const runCommandCaptureSpy = vi
				.spyOn(packageManager as any, "runCommandCapture")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [_command, args] = callArgs as [string, string[]];
					if (args[0] !== "view") {
						throw new Error(`Unexpected runCommandCapture args: ${args.join(" ")}`);
					}
					switch (args[1]) {
						case "user-old":
						case "project-old":
							return '"2.0.0"';
						case "user-current":
						case "project-current":
							return '"1.0.0"';
						case "user-unknown":
							throw new Error("registry unavailable");
						default:
							throw new Error(`Unexpected package lookup: ${args[1]}`);
					}
				});

			let activeNpmUpdates = 0;
			let maxConcurrentNpmUpdates = 0;
			const runCommandSpy = vi
				.spyOn(packageManager as any, "runCommand")
				.mockImplementation(async (...callArgs: unknown[]) => {
					const [command, args] = callArgs as [string, string[]];
					if (command !== "npm") {
						throw new Error(`Unexpected runCommand call: ${command} ${args.join(" ")}`);
					}
					activeNpmUpdates += 1;
					maxConcurrentNpmUpdates = Math.max(maxConcurrentNpmUpdates, activeNpmUpdates);
					await new Promise((resolve) => setTimeout(resolve, 20));
					activeNpmUpdates -= 1;
				});

			let activeGitUpdates = 0;
			let maxConcurrentGitUpdates = 0;
			const updateGitSpy = vi.spyOn(packageManager as any, "updateGit").mockImplementation(async () => {
				activeGitUpdates += 1;
				maxConcurrentGitUpdates = Math.max(maxConcurrentGitUpdates, activeGitUpdates);
				await new Promise((resolve) => setTimeout(resolve, 20));
				activeGitUpdates -= 1;
			});

			await packageManager.update();

			expect(runCommandCaptureSpy).toHaveBeenCalledTimes(5);
			expect(runCommandSpy).toHaveBeenCalledTimes(2);
			expect(runCommandSpy).toHaveBeenNthCalledWith(
				1,
				"npm",
				[
					"install",
					"user-old@latest",
					"user-unknown@latest",
					"--prefix",
					realpathSync(join(agentDir, "npm")),
					"--legacy-peer-deps",
					"--no-audit",
					"--no-fund",
				],
				undefined,
			);
			expect(runCommandSpy).toHaveBeenNthCalledWith(
				2,
				"npm",
				[
					"install",
					"project-old@latest",
					"project-missing@latest",
					"--prefix",
					realpathSync(join(tempDir, ".mehh", "npm")),
					"--legacy-peer-deps",
					"--no-audit",
					"--no-fund",
				],
				undefined,
			);
			expect(updateGitSpy).toHaveBeenCalledTimes(4);
			expect(maxConcurrentNpmUpdates).toBeGreaterThan(1);
			expect(maxConcurrentGitUpdates).toBeGreaterThan(1);
		});

		it("should warn when an npm update leaves the installed version unchanged", async () => {
			const stalePath = join(agentDir, "npm", "node_modules", "stale-pkg");
			const freshPath = join(agentDir, "npm", "node_modules", "fresh-pkg");
			mkdirSync(stalePath, { recursive: true });
			mkdirSync(freshPath, { recursive: true });
			writeFileSync(join(stalePath, "package.json"), JSON.stringify({ name: "stale-pkg", version: "1.0.0" }));
			writeFileSync(join(freshPath, "package.json"), JSON.stringify({ name: "fresh-pkg", version: "1.0.0" }));
			settingsManager.setPackages(["npm:stale-pkg", "npm:fresh-pkg"]);

			vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.0.1"');
			vi.spyOn(packageManager as any, "runCommand").mockImplementation(async () => {
				// Simulate npm resolving stale metadata for stale-pkg only.
				writeFileSync(join(freshPath, "package.json"), JSON.stringify({ name: "fresh-pkg", version: "1.0.1" }));
			});

			const events: ProgressEvent[] = [];
			packageManager.setProgressCallback((event) => events.push(event));

			await packageManager.update();

			const warnings = events.filter((event) => event.type === "progress");
			expect(warnings).toHaveLength(1);
			expect(warnings[0].source).toBe("npm:stale-pkg");
			expect(warnings[0].message).toContain("is still 1.0.0");
			expect(warnings[0].message).toContain("1.0.1");
		});

		it("should suggest npm source prefixes for update lookups", async () => {
			settingsManager.setProjectPackages(["npm:example"]);

			await expect(packageManager.update("example")).rejects.toThrow(
				"No matching package found for example. Did you mean npm:example?",
			);
		});

		it("should suggest git source prefixes for update lookups", async () => {
			settingsManager.setProjectPackages(["git:github.com/example/repo"]);

			await expect(packageManager.update("github.com/example/repo")).rejects.toThrow(
				"No matching package found for github.com/example/repo. Did you mean git:github.com/example/repo?",
			);
		});

		it("should skip installing missing package sources when offline", async () => {
			process.env.MEHH_OFFLINE = "1";
			settingsManager.setProjectPackages(["npm:missing-package", "git:github.com/example/missing-repo"]);

			const installParsedSourceSpy = vi.spyOn(packageManager as any, "installParsedSource");

			const result = await packageManager.resolve();
			const allResources = [...result.extensions, ...result.skills, ...result.prompts, ...result.themes];
			expect(allResources.some((r) => r.metadata.origin === "package")).toBe(false);
			expect(installParsedSourceSpy).not.toHaveBeenCalled();
		});

		it("should load cached git packages without installing when offline", async () => {
			process.env.MEHH_OFFLINE = "1";
			const gitSource = "git:github.com/example/repo";
			const parsedGitSource = (packageManager as any).parseSource(gitSource);
			const installedPath = (packageManager as any).getGitInstallPath(parsedGitSource, "project") as string;

			mkdirSync(join(installedPath, "prompts"), { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "repo", version: "1.0.0" }));
			writeFileSync(join(installedPath, "prompts", "index.md"), "Index");
			settingsManager.setProjectPackages([gitSource]);

			const installParsedSourceSpy = vi.spyOn(packageManager as any, "installParsedSource");

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => pathEndsWith(r.path, "prompts/index.md") && r.enabled)).toBe(true);
			expect(installParsedSourceSpy).not.toHaveBeenCalled();
		});

		it("should install missing git packages when resolving and load their resources", async () => {
			const managerWithInternals = packageManager as unknown as PackageManagerInternals;
			const source = "git:github.com/example/repo@bbbbbbb";
			const parsed = managerWithInternals.parseSource(source);
			if (parsed.type !== "git") {
				throw new Error("Expected git source");
			}

			const installedPath = managerWithInternals.getGitInstallPath(parsed, "project");
			const installParsedSourceSpy = vi
				.spyOn(packageManager as any, "installParsedSource")
				.mockImplementation(async () => {
					mkdirSync(join(installedPath, "prompts"), { recursive: true });
					writeFileSync(join(installedPath, "prompts", "new.md"), "New");
				});

			settingsManager.setProjectPackages([source]);
			const result = await packageManager.resolve();
			expect(installParsedSourceSpy).toHaveBeenCalledTimes(1);
			expect(result.prompts.some((r) => pathEndsWith(r.path, "prompts/new.md") && r.enabled)).toBe(true);
		});

		it("should not run npm view during resolve for installed unpinned packages", async () => {
			process.env.MEHH_OFFLINE = "1";
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(join(installedPath, "prompts"), { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "1.0.0" }));
			writeFileSync(join(installedPath, "prompts", "index.md"), "Index");
			settingsManager.setProjectPackages(["npm:example@^1.0.0"]);

			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture");

			const result = await packageManager.resolve();
			expect(result.prompts.some((r) => pathEndsWith(r.path, "prompts/index.md") && r.enabled)).toBe(true);
			expect(runCommandCaptureSpy).not.toHaveBeenCalled();
		});

		it("should reinstall pinned npm packages when installed version does not match", async () => {
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedPath, { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "1.0.0" }));
			settingsManager.setProjectPackages(["npm:example@2.0.0"]);

			const installParsedSourceSpy = vi
				.spyOn(packageManager as any, "installParsedSource")
				.mockResolvedValue(undefined);

			await packageManager.resolve();
			expect(installParsedSourceSpy).toHaveBeenCalledTimes(1);
		});

		it("should not check package updates when offline", async () => {
			process.env.MEHH_OFFLINE = "1";
			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture");

			const updates = await packageManager.checkForAvailableUpdates();
			expect(updates).toEqual([]);
			expect(runCommandCaptureSpy).not.toHaveBeenCalled();
		});

		it("should report updates for installed unpinned npm packages", async () => {
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedPath, { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "1.0.0" }));
			settingsManager.setProjectPackages(["npm:example"]);

			vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.2.3"');

			const updates = await packageManager.checkForAvailableUpdates();
			expect(updates).toEqual([
				{
					source: "npm:example",
					displayName: "example",
					type: "npm",
					scope: "project",
				},
			]);
		});

		it("should not report npm updates when the installed version is newer than the registry version", async () => {
			const installedPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedPath, { recursive: true });
			writeFileSync(join(installedPath, "package.json"), JSON.stringify({ name: "example", version: "2.0.0" }));
			settingsManager.setProjectPackages(["npm:example"]);

			vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.9.0"');

			const updates = await packageManager.checkForAvailableUpdates();
			expect(updates).toEqual([]);
		});

		it("should skip pinned packages when checking for updates", async () => {
			const installedNpmPath = join(tempDir, ".mehh", "npm", "node_modules", "example");
			mkdirSync(installedNpmPath, { recursive: true });
			writeFileSync(join(installedNpmPath, "package.json"), JSON.stringify({ name: "example", version: "1.0.0" }));
			const parsedGitSource = (packageManager as any).parseSource("git:github.com/example/repo@v1");
			const installedGitPath = (packageManager as any).getGitInstallPath(parsedGitSource, "project") as string;
			mkdirSync(installedGitPath, { recursive: true });

			settingsManager.setProjectPackages(["npm:example@1.0.0", "git:github.com/example/repo@v1"]);

			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture");
			const gitUpdateSpy = vi.spyOn(packageManager as any, "gitHasAvailableUpdate");

			const updates = await packageManager.checkForAvailableUpdates();
			expect(updates).toEqual([]);
			expect(runCommandCaptureSpy).not.toHaveBeenCalled();
			expect(gitUpdateSpy).not.toHaveBeenCalled();
		});

		it("should use npm view to fetch latest version", async () => {
			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.2.3"');

			const latest = await (packageManager as any).getLatestNpmVersion("example");
			expect(latest).toBe("1.2.3");
			expect(runCommandCaptureSpy).toHaveBeenCalledTimes(1);
			expect(runCommandCaptureSpy).toHaveBeenCalledWith(
				"npm",
				["view", "example", "version", "--json"],
				expect.objectContaining({ cwd: tempDir, timeoutMs: expect.any(Number) }),
			);
		});

		it("should use npmCommand argv for npm update checks", async () => {
			settingsManager = SettingsManager.inMemory({
				npmCommand: ["mise", "exec", "node@20", "--", "npm"],
			});
			packageManager = new DefaultPackageManager({
				cwd: tempDir,
				agentDir,
				settingsManager,
			});

			const runCommandCaptureSpy = vi.spyOn(packageManager as any, "runCommandCapture").mockResolvedValue('"1.2.3"');

			const latest = await (packageManager as any).getLatestNpmVersion("@scope/pkg");
			expect(latest).toBe("1.2.3");
			expect(runCommandCaptureSpy).toHaveBeenCalledWith(
				"mise",
				["exec", "node@20", "--", "npm", "view", "@scope/pkg", "version", "--json"],
				expect.objectContaining({ cwd: tempDir }),
			);
		});

		it("should wait for close before resolving captured stdout", async () => {
			const managerWithInternals = packageManager as unknown as {
				spawnCaptureCommand(
					command: string,
					args: string[],
					options?: { cwd?: string; env?: Record<string, string> },
				): MockSpawnedProcess;
				runCommandCapture(
					command: string,
					args: string[],
					options?: { cwd?: string; timeoutMs?: number; env?: Record<string, string> },
				): Promise<string>;
			};
			const child = new MockSpawnedProcess();
			vi.spyOn(managerWithInternals, "spawnCaptureCommand").mockReturnValue(child);

			let settled = false;
			const capturePromise = managerWithInternals.runCommandCapture("git", ["rev-parse", "HEAD"]).then((value) => {
				settled = true;
				return value;
			});

			child.emit("exit", 0, null);
			await Promise.resolve();
			expect(settled).toBe(false);

			child.stdout.write("abc123\n");
			child.stdout.end();
			child.emit("close", 0, null);

			await expect(capturePromise).resolves.toBe("abc123");
		});
	});
});
