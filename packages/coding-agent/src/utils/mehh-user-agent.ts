import { formatUserAgent } from "@mehh-code/ai";

/** mehh's `User-Agent` for its own network requests; the same detailed form @mehh-code/ai sends to providers. */
export function getMehhUserAgent(version: string): string {
	return formatUserAgent({ userAgent: "detailed", version });
}
