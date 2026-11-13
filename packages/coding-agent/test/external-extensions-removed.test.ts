import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { parseArgs } from "../src/cli/args.ts";
import { DefaultPackageManager } from "../src/core/package-manager.ts";
import { DefaultResourceLoader } from "../src/core/resource-loader.ts";
import { SettingsManager } from "../src/core/settings-manager.ts";

describe("external executable extensions are removed", () => {
	let root: string;
	let cwd: string;
	let agentDir: string;

	beforeEach(() => {
		root = mkdtempSync(join(tmpdir(), "mehh-no-external-extensions-"));
		cwd = join(root, "project");
		agentDir = join(root, "agent");
		mkdirSync(cwd, { recursive: true });
		mkdirSync(agentDir, { recursive: true });
	});

	afterEach(() => rmSync(root, { recursive: true, force: true }));

	it.each(["--extension", "-e", "--no-extensions", "-ne"])("rejects the removed %s switch", (flag) => {
		const result = parseArgs([flag, "./plugin.ts"]);
		expect(result.diagnostics.some((diagnostic) => diagnostic.type === "error")).toBe(true);
	});

	it("rejects installing a single-file extension path", async () => {
		const extensionFile = join(root, "my-extension.ts");
		writeFileSync(extensionFile, "export default function() {}");
		const packageManager = new DefaultPackageManager({
			cwd,
			agentDir,
			settingsManager: SettingsManager.inMemory(),
		});

		await expect(packageManager.install(extensionFile)).rejects.toThrow(/must be directories|executable/i);
	});

	it("warns when a configured package declares only mehh.extensions entries", async () => {
		const pkgRoot = join(root, "legacy-extensions-only");
		mkdirSync(join(pkgRoot, "extensions"), { recursive: true });
		writeFileSync(join(pkgRoot, "extensions", "index.ts"), "export default function() {}");
		writeFileSync(
			join(pkgRoot, "package.json"),
			JSON.stringify({ name: "legacy-extensions-only", mehh: { extensions: ["./extensions/index.ts"] } }),
		);

		const loader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager: SettingsManager.inMemory({ packages: [pkgRoot] }),
		});
		await loader.reload();

		expect(loader.getExtensions().extensions).toEqual([]);
		expect((loader.getExtensions().warnings ?? []).some((w) => w.warning.includes("mehh.extensions"))).toBe(true);
	});

	it("warns when a configured local package path is a file", async () => {
		const file = join(root, "single.ts");
		writeFileSync(file, "export default function() {}");

		const loader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager: SettingsManager.inMemory({ packages: [file] }),
		});
		await loader.reload();

		expect((loader.getExtensions().warnings ?? []).some((w) => w.warning.includes("must be a directory"))).toBe(true);
	});

	it("warns about non-builtin entries in the extensions setting", async () => {
		const loader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager: SettingsManager.inMemory({ extensions: ["./my-extension.ts"] }),
		});
		await loader.reload();

		expect((loader.getExtensions().warnings ?? []).some((w) => w.warning.includes("Not supported"))).toBe(true);
	});

	it("does not warn about builtin toggles in the extensions setting", async () => {
		const loader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager: SettingsManager.inMemory({ extensions: ["-builtin:mcp", "+builtin:llama.cpp"] }),
			extensionFactories: [{ name: "llama.cpp", builtin: true, factory: () => {} }],
		});
		await loader.reload();

		expect((loader.getExtensions().warnings ?? []).some((w) => w.warning.includes("Not supported"))).toBe(false);
	});

	it.each([false, true])("never executes external code across reloads (resolve trust: %s)", async (resolveTrust) => {
		const marker = join(root, "executed.txt");
		const code = `import { writeFileSync } from "node:fs";
writeFileSync(${JSON.stringify(marker)}, "executed");
export default function() {}`;
		const userExtensions = join(agentDir, "extensions");
		const projectExtensions = join(cwd, ".mehh", "extensions");
		const packageRoot = join(root, "resource-package");
		for (const directory of [userExtensions, projectExtensions, packageRoot]) {
			mkdirSync(directory, { recursive: true });
			writeFileSync(join(directory, "index.ts"), code);
		}
		const configured = join(root, "configured.ts");
		writeFileSync(configured, code);
		writeFileSync(
			join(packageRoot, "package.json"),
			JSON.stringify({ name: "resource-package", mehh: { extensions: ["index.ts"], prompts: ["review.md"] } }),
		);
		writeFileSync(join(packageRoot, "review.md"), "Review this change.");
		const loader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager: SettingsManager.inMemory({ extensions: [configured], packages: [packageRoot] }),
			extensionFactories: [
				{
					name: "mcp",
					builtin: true,
					factory: (api) => api.registerCommand("mcp", { handler: async () => {} }),
				},
			],
		});
		const options = resolveTrust
			? {
					resolveProjectTrust: async () => {
						expect(existsSync(marker)).toBe(false);
						return true;
					},
				}
			: undefined;

		await loader.reload(options);
		await loader.reload(options);

		expect(existsSync(marker)).toBe(false);
		expect(loader.getExtensions().errors).toEqual([]);
		expect(loader.getExtensions().extensions.map((extension) => [...extension.commands.keys()])).toEqual([["mcp"]]);
		expect(loader.getPrompts().prompts.map((prompt) => prompt.name)).toEqual(["review"]);
	});
});
