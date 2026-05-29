#!/usr/bin/env bash
set -euo pipefail

AUTH_SRC="${OPENCODE_AUTH_JSON:-}"
if [ -z "${OPENCODE_API_KEY:-}" ] && [ -n "$AUTH_SRC" ] && [ -f "$AUTH_SRC" ]; then
  mkdir -p /root/.local/share/opencode
  cp "$AUTH_SRC" /root/.local/share/opencode/auth.json
fi

args=(
  --arms "${AGENT_ARMS:-aft,codegraph}"
  --model "${AGENT_MODEL:-opencode-go/deepseek-v4-flash-free}"
  --fallback-model "${AGENT_FALLBACK_MODEL:-opencode-go/deepseek-v4-pro}"
  --out-dir "${AGENT_OUT_DIR:-results}"
  --timeout-ms "${AGENT_TIMEOUT_MS:-240000}"
)
if [ -n "${AGENT_PROVIDER_BASE_URL:-}" ]; then
  args+=(--provider-base-url "$AGENT_PROVIDER_BASE_URL")
fi
if [ -n "${AGENT_PROVIDER_NAME:-}" ]; then
  args+=(--provider-name "$AGENT_PROVIDER_NAME")
fi
if [ -n "${AGENT_PROVIDER_API_KEY:-}" ]; then
  args+=(--provider-api-key "$AGENT_PROVIDER_API_KEY")
fi
if [ -n "${AGENT_TASK_LIMIT:-}" ]; then
  args+=(--limit "$AGENT_TASK_LIMIT")
fi
if [ "${AGENT_DRY_RUN:-}" = "1" ]; then
  args+=(--dry-run)
fi

exec bun run src/cli.ts "${args[@]}"
