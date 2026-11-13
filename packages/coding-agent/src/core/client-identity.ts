import { type ClientIdentity, MEHH_ATTRIBUTION } from "@mehh-code/ai";
import { VERSION } from "../config.ts";
import type { SettingsManager } from "./settings-manager.ts";
import { isInstallTelemetryEnabled } from "./telemetry.ts";

/**
 * How mehh's requests identify it to providers: a detailed `User-Agent` with mehh's version, and attribution to mehh as a
 * CLI agent while install telemetry is enabled.
 */
export function getClientIdentity(settingsManager: SettingsManager): ClientIdentity {
	return {
		userAgent: "detailed",
		version: VERSION,
		attribution: isInstallTelemetryEnabled(settingsManager) ? MEHH_ATTRIBUTION : null,
	};
}
