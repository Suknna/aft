/**
 * aft_inspect — codebase health snapshot.
 */

import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, isEmptyParam, textResult } from "./_shared.js";
import {
  asNumber,
  asRecord,
  asString,
  extractStructuredPayload,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

const InspectParams = Type.Object({
  sections: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code']). Use 'all' for every active category. Omit for summary-only mode.",
    }),
  ),
  scope: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Restrict scan/results to paths under this scope (file or directory, absolute or relative to project root). Tier 1 scopes the scan; Tier 2 scans project-wide and applies scope as a result filter.",
    }),
  ),
  topK: Type.Optional(
    Type.Integer({
      minimum: 1,
      maximum: 100,
      default: 20,
      description: "Max drill-down items per category. Default 20, max 100.",
    }),
  ),
});

type StringOrStringArray = string | string[];

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

function validateOptionalTopK(value: unknown): number | undefined {
  if (value === undefined || value === null || value === "") return undefined;
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new Error("topK must be an integer between 1 and 100");
  }
  if (value < 1 || value > 100) {
    throw new Error("topK must be between 1 and 100");
  }
  return value;
}

function countFrom(summary: Record<string, unknown> | undefined, key: string): number | undefined {
  const section = asRecord(summary?.[key]);
  return asNumber(section?.count);
}

/** Exported for renderer unit tests. */
export function buildInspectSections(payload: unknown, theme: Theme): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No inspect snapshot available.")];

  const summary = asRecord(response.summary);
  const metrics = asRecord(summary?.metrics);
  const scannerState = asRecord(response.scanner_state);
  const stale = Array.isArray(scannerState?.stale_categories)
    ? scannerState.stale_categories.length
    : 0;
  const pending = Array.isArray(scannerState?.pending_categories)
    ? scannerState.pending_categories.length
    : 0;

  const parts = [
    `todos ${countFrom(summary, "todos") ?? 0}`,
    `metrics ${asNumber(metrics?.files) ?? 0} files/${asNumber(metrics?.symbols) ?? 0} symbols`,
    `dead code ${countFrom(summary, "dead_code") ?? 0}`,
    `unused exports ${countFrom(summary, "unused_exports") ?? 0}`,
    `duplicates ${countFrom(summary, "duplicates") ?? 0}`,
  ];

  const sections = [theme.fg("accent", parts.join(" · "))];
  if (stale > 0 || pending > 0) {
    sections.push(theme.fg("warning", `scanner state: ${stale} stale · ${pending} pending`));
  }

  const details = asRecord(response.details);
  if (details) {
    const names = Object.keys(details);
    sections.push(
      names.length > 0
        ? `details: ${names.join(", ")}`
        : theme.fg("muted", "No drill-down details returned."),
    );
  }

  const text = asString(response.text);
  if (text) sections.push(text);
  return sections;
}

/** Exported for renderer unit tests. */
export function renderInspectCall(
  args: Static<typeof InspectParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const sections = Array.isArray(args.sections)
    ? `${args.sections.length} sections`
    : args.sections;
  const scope = Array.isArray(args.scope) ? `${args.scope.length} scopes` : args.scope;
  const summary = [sections, scope, args.topK ? `topK=${args.topK}` : undefined]
    .filter(Boolean)
    .join(" ");
  return renderToolCall(
    "inspect",
    summary ? theme.fg("toolOutput", summary) : undefined,
    theme,
    context,
  );
}

/** Exported for renderer unit tests. */
export function renderInspectResult(
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "inspect failed", theme, context);
  return renderSections(buildInspectSections(extractStructuredPayload(result), theme), context);
}

export function registerInspectTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_inspect",
    label: "inspect",
    description:
      "Codebase health snapshot. One call returns summary stats for: TODOs, file/symbol metrics, dead code, unused exports, code duplicates. Pass `sections` for per-category drill-down details.\n\n" +
      "Categories run in tiers — Tier 1 (todos, metrics) return synchronously from cache. Tier 2 (dead_code, unused_exports, duplicates) run asynchronously on demand; calls may return cached `stale_categories: [...]` results or `pending_categories: [...]` while the cache warms (waits up to 1s for fresh data before falling back to cached). Pi does not warm Tier 2 automatically between calls.\n\n" +
      "Use when: starting work on unfamiliar code, before a refactor, before review, or to verify cleanup completeness.",
    parameters: InspectParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof InspectParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const sections = normalizeStringOrArray(params.sections);
      const scope = normalizeStringOrArray(params.scope);
      const topK = validateOptionalTopK(params.topK);
      const response = await callBridge(bridge, "inspect", { sections, scope, topK }, extCtx);
      return textResult(
        (response.text as string | undefined) ?? JSON.stringify(response, null, 2),
        response,
      );
    },
    renderCall(args, theme, context) {
      return renderInspectCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderInspectResult(result, theme, context);
    },
  });
}
