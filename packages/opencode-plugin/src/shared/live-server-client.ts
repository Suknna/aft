/**
 * Workaround helper for the OpenCode plugin promptAsync runner-split bug
 * (https://github.com/anomalyco/opencode/issues/28202).
 *
 * OpenCode's plugin-provided `input.client` is constructed with
 * `fetch: async (...args) => Server.Default().app.fetch(...args)`, which
 * routes requests through `HttpApiApp.webHandler()` and a SEPARATE Effect
 * `memoMap` from the one used by the live HTTP listener. Since
 * `SessionRunState` is a per-memo-map in-memory layer, plugin-origin
 * `promptAsync` calls observe an "idle" runner while the live UI turn is
 * still running. The result is that `ensureRunning` fails to coalesce and
 * OpenCode persists multiple assistant children under a single synthetic
 * user parent — what users see as duplicate "stop" messages after every
 * background-bash completion reminder.
 *
 * The workaround is to bypass `input.client` for the wake path and build
 * a separate `createOpencodeClient` configured to hit `input.serverUrl`
 * via `globalThis.fetch`. That client enters the same live listener the
 * UI uses, so the active session's `SessionRunState` is the one that
 * resolves `ensureRunning` and overlapping turns coalesce correctly.
 *
 * Tracked upstream as anomalyco/opencode#28202. When OpenCode fixes the
 * runtime split, this helper and its single consumer in `bg-notifications.ts`
 * can be deleted and the wake path can go back to `input.client`.
 */

import { createOpencodeClient } from "@opencode-ai/sdk";

export type LiveServerClient = ReturnType<typeof createOpencodeClient>;

/**
 * Cache key is `${serverUrl}|${directory}`. Both are stable per OpenCode
 * session/project pair, so one client is reused across many wakes. We don't
 * key on `serverUrl + auth header` because the auth env vars are server-wide
 * — if they change we'd want a fresh client anyway; in practice they're set
 * once at process start.
 */
const clientCache = new Map<string, LiveServerClient>();

function cacheKey(serverUrl: string, directory: string): string {
  return `${serverUrl}|${directory}`;
}

/**
 * Build the Basic-auth header OpenCode's server expects when
 * `OPENCODE_SERVER_PASSWORD` is set. Read at call time (not at module load)
 * so test setup can mutate `process.env` between cases.
 */
function serverAuthHeaders(): Record<string, string> | undefined {
  const password = process.env.OPENCODE_SERVER_PASSWORD;
  if (!password) return undefined;
  const username = process.env.OPENCODE_SERVER_USERNAME ?? "opencode";
  return {
    Authorization: `Basic ${Buffer.from(`${username}:${password}`).toString("base64")}`,
  };
}

/**
 * Return a cached `createOpencodeClient` pointed at the live HTTP listener
 * for the given `(serverUrl, directory)` pair. One client object is reused
 * across many wakes for a given session.
 *
 * The `fetch` is bound to `globalThis.fetch` explicitly. Without this, the
 * SDK would fall back to `globalThis.fetch` anyway in normal Node runtimes,
 * but we set it on purpose so anyone reading this code (or grepping for the
 * bug fix) can see that we intentionally chose the live HTTP transport.
 */
export function getLiveServerClient(serverUrl: string, directory: string): LiveServerClient {
  const key = cacheKey(serverUrl, directory);
  const cached = clientCache.get(key);
  if (cached) return cached;
  const client = createOpencodeClient({
    baseUrl: serverUrl,
    directory,
    headers: serverAuthHeaders(),
    fetch: globalThis.fetch,
  });
  clientCache.set(key, client);
  return client;
}

/** Test helper — drop the cache between cases so each test starts clean. */
export function __resetLiveServerClientCacheForTests(): void {
  clientCache.clear();
}
