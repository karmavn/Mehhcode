import { describe, expect, it } from "vitest";
import { getMehhUserAgent } from "../src/utils/mehh-user-agent.ts";

describe("getMehhUserAgent", () => {
	it("formats the user agent expected by mehh-code.dev", () => {
		const runtime = process.versions.bun ? `bun/${process.versions.bun}` : `node/${process.version}`;
		const userAgent = getMehhUserAgent("1.2.3");

		expect(userAgent).toBe(`mehh/1.2.3 (${process.platform}; ${runtime}; ${process.arch})`);
		expect(userAgent).toMatch(/^mehh\/[^\s()]+ \([^;()]+;\s*[^;()]+;\s*[^()]+\)$/);
	});
});
