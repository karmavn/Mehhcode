import { backgroundAnsi, foregroundAnsi, isAppleTerminalSession, rgbColor } from "@mehh-code/tui";
import { theme } from "../theme/theme.ts";

const GRASS_TOP = rgbColor(143, 232, 78);
const GRASS_MID = rgbColor(85, 166, 43);
const GRASS_EDGE = rgbColor(104, 194, 48);
const DIRT_LIGHT = rgbColor(161, 111, 70);
const DIRT_DARK = rgbColor(133, 87, 51);
const COBBLESTONE = rgbColor(126, 134, 144);
const RESET = "\x1b[0m";

/**
 * The mehh code logo: a Minecraft grass-and-stone block letter M, 5 cells wide and 2 lines tall.
 * Each cell shows two square pixels with half blocks:
 *
 *   grass .     .     .     grass
 *   moss  edge  .     edge  moss
 *   dirt  .     dirt  .     dirt
 *   stone .     .     .     stone
 *
 * The block colors stay fixed across themes; they follow the terminal's color mode.
 */
export function mehhLogoLines(): [string, string] {
	const mode = theme.getColorMode();
	const fg = (color: typeof GRASS_TOP) => foregroundAnsi(color, mode);
	const bg = (color: typeof GRASS_TOP) => backgroundAnsi(color, mode);
	const top =
		`${fg(GRASS_TOP)}${bg(GRASS_MID)}▀${RESET}` +
		`${fg(GRASS_EDGE)}▄${RESET} ` +
		`${fg(GRASS_EDGE)}▄${RESET}` +
		`${fg(GRASS_TOP)}${bg(GRASS_MID)}▀${RESET}`;
	const bottom =
		`${fg(DIRT_DARK)}${bg(COBBLESTONE)}▀${RESET} ` +
		`${fg(DIRT_LIGHT)}▀${RESET} ` +
		`${fg(DIRT_DARK)}${bg(COBBLESTONE)}▀${RESET}`;
	return [top, bottom];
}

/**
 * Whether the terminal renders the half-block logo correctly. Apple Terminal draws gaps between rows and
 * misaligns the half blocks, so it gets the text wordmark instead.
 */
export function supportsMehhLogo(): boolean {
	return !isAppleTerminalSession();
}

/** Text fallback for the logo: "Mehh" in Minecraft grass green and earth brown. */
export function mehhWordmark(): string {
	const mode = theme.getColorMode();
	return `${foregroundAnsi(GRASS_TOP, mode)}M${RESET}${foregroundAnsi(DIRT_LIGHT, mode)}ehh${RESET}`;
}
