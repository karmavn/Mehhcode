import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.ts";
import { createEventBus } from "../src/core/event-bus.ts";
import { createExtensionRuntime, loadExtensionFromFactory } from "../src/core/extensions/loader.ts";
import { ExtensionRunner } from "../src/core/extensions/runner.ts";
import type { ExtensionAPI, ExtensionFactory } from "../src/core/extensions/types.ts";
import { SessionManager } from "../src/core/session-manager.ts";

import { createInMemoryModelRegistry } from "./model-runtime-test-utils.ts";

describe("Input Event", () => {
	const tempDir = "input-event-test";

	beforeEach(() => {
		// Clean globalThis test vars
		delete (globalThis as Record<string, unknown>).testVar;
	});

	afterEach(() => {
		delete (globalThis as Record<string, unknown>).testVar;
	});

	async function createRunner(...factories: ExtensionFactory[]) {
		const runtime = createExtensionRuntime();
		const eventBus = createEventBus();
		const extensions = await Promise.all(
			factories.map((factory, index) =>
				loadExtensionFromFactory(factory, tempDir, eventBus, runtime, `<inline:input-event-${index}>`),
			),
		);
		const sm = SessionManager.inMemory();
		const mr = await createInMemoryModelRegistry(AuthStorage.inMemory());
		return new ExtensionRunner(extensions, runtime, tempDir, sm, mr);
	}

	const setTestVar = (value: unknown) => {
		(globalThis as Record<string, unknown>).testVar = value;
	};
	const getTestVar = () => (globalThis as Record<string, unknown>).testVar;

	it("returns continue when no handlers, undefined return, or explicit continue", async () => {
		// No handlers
		expect((await (await createRunner()).emitInput("x", undefined, "interactive")).action).toBe("continue");
		// Returns undefined
		let r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async () => {});
		});
		expect((await r.emitInput("x", undefined, "interactive")).action).toBe("continue");
		// Returns explicit continue
		r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async () => ({ action: "continue" }));
		});
		expect((await r.emitInput("x", undefined, "interactive")).action).toBe("continue");
	});

	it("transforms text and preserves images when omitted", async () => {
		const r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async (event) => ({ action: "transform", text: `T:${event.text}` }));
		});
		const imgs = [{ type: "image" as const, data: "orig", mimeType: "image/png" }];
		const result = await r.emitInput("hi", imgs, "interactive");
		expect(result).toEqual({ action: "transform", text: "T:hi", images: imgs });
	});

	it("transforms and replaces images when provided", async () => {
		const r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async () => ({
				action: "transform",
				text: "X",
				images: [{ type: "image" as const, data: "new", mimeType: "image/jpeg" }],
			}));
		});
		const result = await r.emitInput("hi", [{ type: "image", data: "orig", mimeType: "image/png" }], "interactive");
		expect(result).toEqual({
			action: "transform",
			text: "X",
			images: [{ type: "image", data: "new", mimeType: "image/jpeg" }],
		});
	});

	it("chains transforms across multiple handlers", async () => {
		const r = await createRunner(
			(p: ExtensionAPI) => {
				p.on("input", async (event) => ({ action: "transform", text: `${event.text}[1]` }));
			},
			(p: ExtensionAPI) => {
				p.on("input", async (event) => ({ action: "transform", text: `${event.text}[2]` }));
			},
		);
		const result = await r.emitInput("X", undefined, "interactive");
		expect(result).toEqual({ action: "transform", text: "X[1][2]", images: undefined });
	});

	it("short-circuits on handled and skips subsequent handlers", async () => {
		setTestVar(false);
		const r = await createRunner(
			(p: ExtensionAPI) => {
				p.on("input", async () => ({ action: "handled" }));
			},
			(p: ExtensionAPI) => {
				p.on("input", async () => {
					setTestVar(true);
				});
			},
		);
		expect(await r.emitInput("X", undefined, "interactive")).toEqual({ action: "handled" });
		expect(getTestVar()).toBe(false);
	});

	it("passes source correctly for all source types", async () => {
		const r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async (event) => {
				setTestVar(event.source);
				return { action: "continue" };
			});
		});
		for (const source of ["interactive", "rpc", "extension"] as const) {
			await r.emitInput("x", undefined, source);
			expect(getTestVar()).toBe(source);
		}
	});

	it("passes streamingBehavior correctly", async () => {
		const r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async (event) => {
				setTestVar(event.streamingBehavior);
				return { action: "continue" };
			});
		});
		await r.emitInput("x", undefined, "interactive", "steer");
		expect(getTestVar()).toBe("steer");
		await r.emitInput("x", undefined, "interactive", "followUp");
		expect(getTestVar()).toBe("followUp");
		await r.emitInput("x", undefined, "interactive");
		expect(getTestVar()).toBeUndefined();
	});

	it("catches handler errors and continues", async () => {
		const r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async () => {
				throw new Error("boom");
			});
		});
		const errs: string[] = [];
		r.onError((e) => errs.push(e.error));
		const result = await r.emitInput("x", undefined, "interactive");
		expect(result.action).toBe("continue");
		expect(errs).toContain("boom");
	});

	it("hasHandlers returns correct value", async () => {
		let r = await createRunner();
		expect(r.hasHandlers("input")).toBe(false);
		r = await createRunner((p: ExtensionAPI) => {
			p.on("input", async () => {});
		});
		expect(r.hasHandlers("input")).toBe(true);
	});
});
