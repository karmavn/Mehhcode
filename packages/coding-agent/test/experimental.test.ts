import { afterEach, describe, expect, it } from "vitest";
import { areExperimentalFeaturesEnabled } from "../src/core/experimental.ts";

describe("areExperimentalFeaturesEnabled", () => {
	const originalMehhExperimental = process.env.MEHH_EXPERIMENTAL;

	afterEach(() => {
		if (originalMehhExperimental === undefined) {
			delete process.env.MEHH_EXPERIMENTAL;
		} else {
			process.env.MEHH_EXPERIMENTAL = originalMehhExperimental;
		}
	});

	it("returns false when MEHH_EXPERIMENTAL is unset", () => {
		delete process.env.MEHH_EXPERIMENTAL;

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});

	it("returns false when MEHH_EXPERIMENTAL is empty", () => {
		process.env.MEHH_EXPERIMENTAL = "";

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});

	it("returns true when MEHH_EXPERIMENTAL is set to 1", () => {
		process.env.MEHH_EXPERIMENTAL = "1";

		expect(areExperimentalFeaturesEnabled()).toBe(true);
	});

	it("returns false when MEHH_EXPERIMENTAL is set to 0", () => {
		process.env.MEHH_EXPERIMENTAL = "0";

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});

	it("returns false when MEHH_EXPERIMENTAL is set to a non-1 value", () => {
		process.env.MEHH_EXPERIMENTAL = "true";

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});
});
