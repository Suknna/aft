import * as fs from "node:fs/promises";
import type { BridgeRequestOptions } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import {
  consumeBgCompletion,
  markExplicitControl,
  markTaskWaiting,
  unmarkTaskWaiting,
} from "../bg-notifications.js";
import { resolveBashConfig } from "../config.js";
import { getOrCreatePtyTerminal, readPtyBytes } from "../shared/pty-cache.js";
import { resolveIsSubagent } from "../shared/subagent-detect.js";
import type { PluginContext } from "../types.js";
import { callBridge, projectRootFor } from "./_shared.js";

const z = tool.schema;
const BASH_WAIT_POLL_INTERVAL_MS = 100;
const DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS = 30_000;
const MAX_BASH_STATUS_WAIT_TIMEOUT_MS = 300_000;
const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

export type BashWaitPattern =
  | { kind: "substring"; value: string }
  | { kind: "regex"; value: RegExp; source: string };
export type BashStatusWaited = {
  reason: "matched" | "exited" | "timeout";
  elapsed_ms: number;
  match?: string;
  match_offset?: number;
};
type BashStatusWithWait = Record<string, unknown> & { waited?: BashStatusWaited };

export function createBashWatchTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      "Block on a background bash task until a pattern matches, it exits, or timeout elapses; or register an async pattern notification with background:true.",
    args: {
      taskId: z.string().describe("Background task ID returned by bash({ background: true })."),
      pattern: z
        .union([z.string(), z.object({ regex: z.string() })])
        .optional()
        .describe(
          "Substring or regex pattern. Optional in sync mode; required with background:true.",
        ),
      background: z
        .boolean()
        .optional()
        .describe(
          "When true, register an async watch and return immediately. Defaults to false (sync wait).",
        ),
      timeoutMs: z
        .number()
        .int()
        .positive()
        .optional()
        .describe("Sync-only timeout in milliseconds. Default 30000, max 300000."),
      once: z
        .boolean()
        .optional()
        .describe("Async-only. Defaults true; false keeps the watch sticky until task exit."),
    },
    execute: async (args, context) => {
      const taskId = args.taskId as string;
      const requestedAsync = args.background === true;
      const waitFor = parseWaitPattern(args.pattern);
      const bashCfg = resolveBashConfig(ctx.config);
      const isSubagent = await resolveIsSubagent(ctx.client, context.sessionID, context.directory);
      const subagentForcedSync = requestedAsync && isSubagent && !bashCfg.subagent_background;
      const asyncMode = requestedAsync && !subagentForcedSync;

      if (asyncMode) {
        if (!waitFor) {
          throw new Error(
            "invalid_request: Use auto-reminder; bash_watch without pattern in async mode is redundant",
          );
        }
        const notifyParams: Record<string, unknown> = {
          task_id: taskId,
          once: args.once !== false,
        };
        if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
        else notifyParams.pattern = waitFor.value;
        const registered = await callBridge(ctx, context, "bash_notify", notifyParams);
        if (registered.success === false) {
          const code = String(registered.code ?? "invalid_request");
          const message = String(registered.message ?? "bash_notify failed");
          if (code === "too_many_watches") throw new Error(`invalid_request: ${message}`);
          throw new Error(`${code}: ${message}`);
        }
        markExplicitControl(context.sessionID, taskId);
        const snapshot = await bashStatusSnapshot(ctx, context, taskId, undefined);
        return JSON.stringify(
          { registered: true, watchId: registered.watch_id, snapshot },
          null,
          2,
        );
      }

      const effectiveWaitMs = subagentForcedSync
        ? MAX_BASH_STATUS_WAIT_TIMEOUT_MS
        : Math.min(
            (args.timeoutMs as number | undefined) ?? DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS,
            MAX_BASH_STATUS_WAIT_TIMEOUT_MS,
          );
      return JSON.stringify(
        await waitForBashStatus(ctx, context, taskId, undefined, waitFor, effectiveWaitMs),
        null,
        2,
      );
    },
  };
}

async function bashStatusSnapshot(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const data = await callBridge(
    ctx,
    runtime,
    "bash_status",
    { task_id: taskId, output_mode: outputMode },
    options,
  );
  if (data.success === false)
    throw new Error((data.message as string | undefined) ?? "bash_status failed");
  return data;
}

export async function waitForBashStatus(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  waitFor: BashWaitPattern | undefined,
  effectiveWaitMs: number,
): Promise<BashStatusWithWait> {
  const startedAt = Date.now();
  const deadline = startedAt + effectiveWaitMs;
  let spillCursor = 0;
  let scanText = "";
  let scanBaseOffset = 0;
  const bridgeOptions: BridgeRequestOptions = {
    keepBridgeOnTimeout: true,
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
  };
  markTaskWaiting(runtime.sessionID, taskId);
  try {
    while (true) {
      const data = await bashStatusSnapshot(ctx, runtime, taskId, outputMode, bridgeOptions);
      if (isTerminalStatus(data.status)) {
        consumeBgCompletion(runtime.sessionID, taskId);
        return withWaited(data, { reason: "exited", elapsed_ms: Date.now() - startedAt });
      }
      if (waitFor) {
        const scan = await readNewTaskOutput(runtime, taskId, data, spillCursor);
        if (scan) {
          spillCursor = scan.nextCursor;
          if (scanText.length === 0) scanBaseOffset = scan.baseOffset;
          scanText += scan.text;
          const match = findWaitMatch(scanText, waitFor);
          if (match) {
            unmarkTaskWaiting(runtime.sessionID, taskId);
            return withWaited(data, {
              reason: "matched",
              elapsed_ms: Date.now() - startedAt,
              match: match.text,
              match_offset:
                scanBaseOffset + Buffer.byteLength(scanText.slice(0, match.index), "utf8"),
            });
          }
        }
      }
      if (Date.now() >= deadline) {
        unmarkTaskWaiting(runtime.sessionID, taskId);
        return withWaited(data, { reason: "timeout", elapsed_ms: Date.now() - startedAt });
      }
      await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
    }
  } catch (err) {
    unmarkTaskWaiting(runtime.sessionID, taskId);
    throw err;
  }
}

async function readNewTaskOutput(
  runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  cursor: number,
): Promise<{ text: string; baseOffset: number; nextCursor: number } | undefined> {
  const outputPath = data.output_path as string | undefined;
  if (!outputPath) return undefined;
  if (data.mode === "pty") {
    const state = await getOrCreatePtyTerminal(ptyCacheKey(runtime, taskId), outputPath);
    const baseOffset = state.offset;
    const bytes = await readPtyBytes(state);
    return { text: bytes.toString("utf8"), baseOffset, nextCursor: state.offset };
  }
  const bytes = await readFileBytesFrom(outputPath, cursor);
  return { text: bytes.toString("utf8"), baseOffset: cursor, nextCursor: cursor + bytes.length };
}

async function readFileBytesFrom(outputPath: string, cursor: number): Promise<Buffer> {
  const handle = await fs.open(outputPath, "r");
  try {
    const chunks: Buffer[] = [];
    let offset = cursor;
    while (true) {
      const buffer = Buffer.allocUnsafe(64 * 1024);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, offset);
      if (bytesRead === 0) break;
      chunks.push(Buffer.from(buffer.subarray(0, bytesRead)));
      offset += bytesRead;
    }
    return Buffer.concat(chunks);
  } finally {
    await handle.close().catch(() => undefined);
  }
}

export function parseWaitPattern(value: unknown): BashWaitPattern | undefined {
  if (typeof value === "string") return { kind: "substring", value };
  if (isRegexWaitObject(value))
    return { kind: "regex", value: new RegExp(value.regex), source: value.regex };
  return undefined;
}
function isRegexWaitObject(value: unknown): value is { regex: string } {
  return (
    typeof value === "object" &&
    value !== null &&
    "regex" in value &&
    typeof (value as { regex?: unknown }).regex === "string"
  );
}
function findWaitMatch(
  text: string,
  pattern: BashWaitPattern,
): { text: string; index: number } | undefined {
  if (pattern.kind === "substring") {
    const index = text.indexOf(pattern.value);
    return index >= 0 ? { text: pattern.value, index } : undefined;
  }
  pattern.value.lastIndex = 0;
  const match = pattern.value.exec(text);
  return match ? { text: match[0], index: match.index } : undefined;
}
function withWaited(data: Record<string, unknown>, waited: BashStatusWaited): BashStatusWithWait {
  return { ...data, waited };
}
function isTerminalStatus(status: unknown): boolean {
  return (
    status === "completed" || status === "failed" || status === "killed" || status === "timed_out"
  );
}
function ptyCacheKey(runtime: ToolContext, taskId: string): string {
  return `${projectRootFor(runtime)}::${runtime.sessionID ?? "__default__"}::${taskId}`;
}
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
