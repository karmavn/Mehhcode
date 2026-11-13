export function areExperimentalFeaturesEnabled(): boolean {
	return process.env.MEHH_EXPERIMENTAL === "1";
}
