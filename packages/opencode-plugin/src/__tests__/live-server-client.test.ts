/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import {
  __resetLiveServerWakeForTests,
  probeServerReachable,
  setLiveServerWakeAvailable,
  useLiveServerWake,
} from "../shared/live-server-client.js";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
  __resetLiveServerWakeForTests();
});

describe("probeServerReachable", () => {
  test("accepts successful OpenCode API responses", async () => {
    stubFetch(204);

    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(true);
  });

  test("accepts auth-protected listeners but rejects plain-TUI 404s", async () => {
    stubFetch(401);
    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(true);

    stubFetch(403);
    await expect(probeServerReachable("http://127.0.0.1:4097/")).resolves.toBe(true);

    stubFetch(404);
    await expect(probeServerReachable("http://127.0.0.1:4098/")).resolves.toBe(false);
  });

  test("records reachability per serverUrl", async () => {
    setLiveServerWakeAvailable("http://127.0.0.1:4096/", true);
    setLiveServerWakeAvailable("http://127.0.0.1:4097/", false);

    expect(useLiveServerWake("http://127.0.0.1:4096/")).toBe(true);
    expect(useLiveServerWake("http://127.0.0.1:4097/")).toBe(false);
    expect(useLiveServerWake("http://127.0.0.1:4098/")).toBe(false);
  });

  test("probe results do not cross-contaminate other serverUrls", async () => {
    stubFetch(204);
    await expect(probeServerReachable("http://127.0.0.1:4096/")).resolves.toBe(true);

    expect(useLiveServerWake("http://127.0.0.1:4096/")).toBe(true);
    expect(useLiveServerWake("http://127.0.0.1:4097/")).toBe(false);
  });
});

function stubFetch(status: number): void {
  globalThis.fetch = (async () => new Response(null, { status })) as typeof fetch;
}
