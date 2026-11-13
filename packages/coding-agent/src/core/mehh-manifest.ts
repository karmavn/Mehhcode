import { readFileSync } from "node:fs";
import { stripBom } from "../utils/text.ts";

export interface MehhManifest {
	/** Parsed but ignored: executable extension entries no longer load from packages. */
	extensions?: string[];
	skills?: string[];
	prompts?: string[];
	themes?: string[];
	changelogPath?: string;
}

const RESOURCE_FIELDS = ["extensions", "skills", "prompts", "themes"] as const;

function isObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function readMehhManifest(packageJsonPath: string): MehhManifest | null {
	try {
		const pkg: unknown = JSON.parse(stripBom(readFileSync(packageJsonPath, "utf-8")));
		if (!isObject(pkg) || !isObject(pkg.mehh)) {
			return null;
		}

		const manifest: MehhManifest = {};
		for (const field of RESOURCE_FIELDS) {
			const entries = pkg.mehh[field];
			if (Array.isArray(entries) && entries.every((entry) => typeof entry === "string")) {
				manifest[field] = entries;
			}
		}

		const changelogPath = pkg.mehh.changelogPath;
		if (typeof changelogPath === "string") {
			manifest.changelogPath = changelogPath;
		}

		return manifest;
	} catch {
		return null;
	}
}
