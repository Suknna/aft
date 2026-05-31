import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam, resolvePathArg } from "./_shared.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

type ToolArg = ToolDefinition["args"][string];

type StringOrStringArray = string | string[];

function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function asStringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

function diagnosticsServerSummary(section: Record<string, unknown>): string {
  const pending = asStringArray(section.servers_pending);
  const notInstalled = asStringArray(section.servers_not_installed);
  const parts: string[] = [];
  if (pending.length > 0) parts.push(`pending: ${pending.join(", ")}`);
  if (notInstalled.length > 0) parts.push(`not installed: ${notInstalled.join(", ")}`);
  return parts.length > 0 ? parts.join("; ") : "none reported";
}

function formatDiagnosticsSummary(
  summary: Record<string, unknown> | undefined,
): string | undefined {
  const section = asRecord(summary?.diagnostics);
  if (!section) return undefined;

  const errors = asNumber(section.errors);
  const warnings = asNumber(section.warnings);
  const info = asNumber(section.info);
  const hints = asNumber(section.hints);
  const hasCounts = [errors, warnings, info, hints].some((value) => value !== undefined);
  const counts = `${errors ?? 0} errors, ${warnings ?? 0} warnings, ${info ?? 0} info, ${hints ?? 0} hints`;
  const status = asString(section.status);

  // Partial result: counts found SO FAR are present alongside a status/gap
  // signal. Show both — the counts are real (e.g. one server already
  // reported) and the status tells the agent more may still arrive, so the
  // counts must not be read as the final/complete picture.
  if (status === "pending") {
    return hasCounts
      ? `diagnostics: ${counts} so far — still pending (servers: ${diagnosticsServerSummary(section)})`
      : `diagnostics: pending (servers: ${diagnosticsServerSummary(section)})`;
  }
  if (status === "incomplete") {
    return hasCounts
      ? `diagnostics: ${counts} (incomplete — servers: ${diagnosticsServerSummary(section)})`
      : `diagnostics: unavailable (status incomplete; servers: ${diagnosticsServerSummary(section)})`;
  }

  // Complete result: counts are the full, trustworthy picture.
  if (hasCounts) {
    return `diagnostics: ${counts}`;
  }

  return undefined;
}

function formatDiagnosticLocation(diagnostic: Record<string, unknown>): string {
  const file = asString(diagnostic.file) ?? "(unknown file)";
  const line = asNumber(diagnostic.line);
  const column = asNumber(diagnostic.column);
  if (line === undefined) return file;
  if (column === undefined) return `${file}:${line}`;
  return `${file}:${line}:${column}`;
}

function formatDiagnosticsDetails(details: Record<string, unknown> | undefined): string[] {
  const diagnostics = Array.isArray(details?.diagnostics)
    ? (details.diagnostics.map(asRecord).filter(Boolean) as Record<string, unknown>[])
    : [];
  return diagnostics.map((diagnostic) => {
    const severity = asString(diagnostic.severity) ?? "information";
    const message = asString(diagnostic.message) ?? "(no message)";
    const source = asString(diagnostic.source);
    const suffix = source ? ` [${source}]` : "";
    return `${formatDiagnosticLocation(diagnostic)} ${severity} ${message}${suffix}`;
  });
}

export function renderInspectDiagnostics(response: Record<string, unknown>): string {
  const lines: string[] = [];
  const summaryLine = formatDiagnosticsSummary(asRecord(response.summary));
  if (summaryLine) lines.push(summaryLine);

  const detailLines = formatDiagnosticsDetails(asRecord(response.details));
  if (detailLines.length > 0) {
    lines.push("diagnostics details:", ...detailLines.map((line) => `- ${line}`));
  }

  return lines.join("\n");
}

function appendRenderedDiagnostics(text: string, response: Record<string, unknown>): string {
  if (/^diagnostics[: ]/im.test(text)) return text;
  const diagnostics = renderInspectDiagnostics(response);
  if (!diagnostics) return text;
  return text ? `${text}\n\n${diagnostics}` : diagnostics;
}

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

async function resolveAndGateScope(
  ctx: PluginContext,
  context: Parameters<ToolDefinition["execute"]>[1],
  scope: StringOrStringArray | undefined,
): Promise<{ scope: StringOrStringArray | undefined; denial?: string }> {
  if (scope === undefined) return { scope: undefined };
  const values = Array.isArray(scope) ? scope : [scope];
  const resolved = await Promise.all(
    values
      .filter((value): value is string => typeof value === "string" && value.length > 0)
      .map((value) => resolvePathArg(ctx, context, value)),
  );
  const checked = new Set<string>();
  for (const target of resolved) {
    if (checked.has(target)) continue;
    checked.add(target);
    const denial = await assertExternalDirectoryPermission(context, target);
    if (denial) return { scope: undefined, denial };
  }
  return { scope: Array.isArray(scope) ? resolved : resolved[0] };
}

export interface InspectToolConfig {
  tool_surface?: "minimal" | "recommended" | "all";
  disabled_tools?: string[];
  inspect?: {
    enabled?: boolean;
    tier2_idle_minutes?: number;
  };
}

export function inspectToolSurfaceEnabled(config: InspectToolConfig): boolean {
  return (config.tool_surface ?? "recommended") !== "minimal" && config.inspect?.enabled !== false;
}

export function shouldRegisterInspectTool(config: InspectToolConfig): boolean {
  return (
    inspectToolSurfaceEnabled(config) && !(config.disabled_tools ?? []).includes("aft_inspect")
  );
}

type TimerHandle = ReturnType<typeof setTimeout>;

export interface InspectTier2IdleSchedulerOptions {
  isEnabled: () => boolean;
  idleMinutes: () => number | undefined;
  run: (sessionID: string) => Promise<void>;
  warn?: (message: string) => void;
  setTimer?: (callback: () => void, delayMs: number) => TimerHandle;
  clearTimer?: (timer: TimerHandle) => void;
}

export function createInspectTier2IdleScheduler(options: InspectTier2IdleSchedulerOptions) {
  const timers = new Map<string, TimerHandle>();
  const setTimer = options.setTimer ?? ((callback, delayMs) => setTimeout(callback, delayMs));
  const clearTimer = options.clearTimer ?? ((timer) => clearTimeout(timer));

  const clear = (sessionID: string): void => {
    const timer = timers.get(sessionID);
    if (!timer) return;
    clearTimer(timer);
    timers.delete(sessionID);
  };

  const clearAll = (): void => {
    for (const timer of timers.values()) {
      clearTimer(timer);
    }
    timers.clear();
  };

  const schedule = (sessionID: string): void => {
    if (!options.isEnabled()) return;
    clear(sessionID);
    const idleMinutes = options.idleMinutes() ?? 4;
    const delayMs = Math.max(0, idleMinutes * 60 * 1000);
    const timer = setTimer(() => {
      timers.delete(sessionID);
      options.run(sessionID).catch((err) => {
        options.warn?.(
          `inspect_tier2_run failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      });
    }, delayMs);
    timers.set(sessionID, timer);
  };

  return { schedule, clear, clearAll };
}

export function inspectTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const inspectTool: ToolDefinition = {
    description:
      "Codebase health snapshot. One call returns summary stats for: TODOs, diagnostics, file/symbol metrics, dead code, unused exports, code duplicates. Pass `sections` for per-category drill-down details.\n\n" +
      "Categories run in tiers — Tier 1 (todos, metrics) return synchronously from cache. Tier 2 (dead_code, unused_exports, duplicates) run as background scans triggered on session idle; calls may return cached `stale_categories: [...]` results or `pending_categories: [...]` while a refresh is in progress (waits up to 1s for fresh data before falling back to cached).\n\n" +
      "Use when: starting work on unfamiliar code, after multi-edit batches to check diagnostics, before a refactor, before review, or to verify cleanup completeness.\n\n" +
      "Treat `dead_code` as a hint, not proof: reachability is call-based, so symbols reached only via method dispatch or referenced only in type position may be false positives — verify before deleting.",
    args: {
      sections: arg(
        z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code']). Use 'all' for every active category. Omit for summary-only mode.",
          ),
      ),
      scope: arg(
        z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Restrict scan/results to paths under this scope (file or directory, absolute or relative to project root). Tier 1 scopes the scan; Tier 2 scans project-wide and applies scope as a result filter.",
          ),
      ),
      topK: arg(
        z
          .number()
          .int()
          .positive()
          .max(100)
          .optional()
          .describe("Max drill-down items per category. Default 20, max 100."),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const sections = normalizeStringOrArray(args.sections);
      const scoped = await resolveAndGateScope(ctx, context, normalizeStringOrArray(args.scope));
      if (scoped.denial) return permissionDeniedResponse(scoped.denial);
      const scope = scoped.scope;
      const topK = args.topK === undefined || args.topK === null ? undefined : args.topK;

      const response = await callBridge(ctx, context, "inspect", { sections, scope, topK });
      if (response.success === false) {
        throw new Error((response.message as string) || "inspect failed");
      }
      if (typeof response.text === "string") {
        return appendRenderedDiagnostics(response.text, response);
      }
      const diagnostics = renderInspectDiagnostics(response);
      const json = JSON.stringify(response, null, 2);
      return diagnostics
        ? `${json}

${diagnostics}`
        : json;
    },
  };

  return {
    aft_inspect: inspectTool,
  };
}
