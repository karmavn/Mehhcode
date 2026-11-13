import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { createEventBus } from "../src/core/event-bus.ts";
import { createExtensionRuntime, loadExtensionFromFactory } from "../src/core/extensions/loader.ts";
import type { Extension, ExtensionFactory, LoadExtensionsResult } from "../src/core/extensions/types.ts";

/**
 * Test-only fixture loader. Fixtures are trusted strings written as
 * `export default function (mehh) { ... }` and are never discovered by production code.
 */
function compileFixture(source: string, path: string): ExtensionFactory {
	const body = source.replace(/\bexport\s+default\s+/, "return ");
	if (body === source) {
		throw new Error(`Fixture ${path} has no default export`);
	}
	return new Function(body)() as ExtensionFactory;
}

export async function loadExtensionFixtures(paths: string[], cwd: string): Promise<LoadExtensionsResult> {
	const runtime = createExtensionRuntime();
	const eventBus = createEventBus();
	const extensions: Extension[] = [];
	const errors: Array<{ path: string; error: string }> = [];
	for (const path of paths) {
		try {
			const factory = compileFixture(readFileSync(path, "utf-8"), path);
			extensions.push(await loadExtensionFromFactory(factory, cwd, eventBus, runtime, path));
		} catch (error) {
			errors.push({
				path,
				error: `Failed to load extension: ${error instanceof Error ? error.message : String(error)}`,
			});
		}
	}
	return { extensions, errors, runtime };
}

export async function loadExtensionFixtureDir(dir: string, cwd: string): Promise<LoadExtensionsResult> {
	const paths = readdirSync(dir)
		.filter((name) => name.endsWith(".ts") || name.endsWith(".js"))
		.sort()
		.map((name) => join(dir, name));
	return loadExtensionFixtures(paths, cwd);
}
