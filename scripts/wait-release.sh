#!/usr/bin/env bash
# Wait for the GitHub Actions release workflow to complete for a given tag.
# Usage: ./scripts/wait-release.sh v0.7.5 [--max-wait <seconds>] [--no-fail-fast]
# Polls every 5 seconds, exits 0 on success, 1 on failure, 2 on timeout.
#
# Behavior:
#   - Watches BOTH the workflow conclusion AND individual job state. When any
#     non-skipped, non-allowlisted job reaches a failure-class conclusion
#     (failure, cancelled, timed_out, action_required) the script cancels
#     the remaining run, waits for terminal state, and exits 1 with the
#     failing job name. This saves the 15-20 minute tail when one job fails
#     at minute 3 but the other parallel jobs would have kept running.
#   - Allowlist of failure-tolerant jobs is fixed in JOB_FAILURE_ALLOWLIST
#     below — only contains the Discord announcement step, which is
#     intentionally continue-on-error and isn't allowed to roll back a real
#     release.
#   - Use --no-fail-fast to disable cancel-on-first-failure (script will then
#     wait for natural workflow completion regardless of any job failure).
#   - Output uses plain newlines so line-buffered pipes flush every status
#     update. Per-job transitions are printed once so callers see progress.
#   - Default max-wait is 35 minutes (2100s). Healthy releases finish in
#     ~13-26 min; 35 min gives headroom for v0.30+ workflows where Windows
#     builds plus npm publish push the total past 25 min. Override with
#     --max-wait or MAX_WAIT_SECONDS env var.

set -euo pipefail

TAG=""
MAX_WAIT="${MAX_WAIT_SECONDS:-2100}"
REPO="cortexkit/aft"
INTERVAL=5
FAIL_FAST=1

# Jobs allowed to fail without rolling back the release.
# This is a substring match against the job name in `gh run view --json jobs`.
# Keep this list narrow — only jobs with `continue-on-error: true` that are
# definitionally non-blocking belong here.
JOB_FAILURE_ALLOWLIST=(
  "Announce on Discord"
)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --max-wait)
      MAX_WAIT="$2"
      shift 2
      ;;
    --no-fail-fast)
      FAIL_FAST=0
      shift
      ;;
    -*)
      echo "Unknown flag: $1" >&2
      exit 64
      ;;
    *)
      if [[ -z "$TAG" ]]; then
        TAG="$1"
      else
        echo "Unexpected positional argument: $1" >&2
        exit 64
      fi
      shift
      ;;
  esac
done

if [[ -z "$TAG" ]]; then
  echo "Usage: wait-release.sh <tag> [--max-wait <seconds>] [--no-fail-fast]" >&2
  exit 64
fi

if (( FAIL_FAST == 1 )); then
  echo "⏳ Waiting for release workflow on ${TAG} (max ${MAX_WAIT}s, fail-fast on)..."
else
  echo "⏳ Waiting for release workflow on ${TAG} (max ${MAX_WAIT}s, fail-fast off)..."
fi

if ! GH_AUTH_OUTPUT=$(gh auth status --hostname github.com 2>&1); then
  echo "ERROR: gh auth status failed:" >&2
  echo "$GH_AUTH_OUTPUT" >&2
  exit 1
fi

START_TIME=$(date +%s)
GH_API_FAILURES=0
MAX_GH_API_FAILURES=3

# Track per-job state so we only print transitions, not the full table every poll.
# Bash 3 (macOS default) doesn't ship associative arrays portably, so we serialise
# state into a tab-separated string and grep it. Format: "<name>\t<state>\n"
PRINTED_STATES=""

gh_run_list() {
  gh run list --repo "$REPO" --workflow release.yml --branch "$TAG" --limit 1 --json status,conclusion,databaseId
}

gh_run_jobs() {
  local run_id="$1"
  gh run view --repo "$REPO" "$run_id" --json jobs
}

is_allowlisted_job() {
  local name="$1"
  for allow in "${JOB_FAILURE_ALLOWLIST[@]}"; do
    if [[ "$name" == *"$allow"* ]]; then
      return 0
    fi
  done
  return 1
}

# Returns 0 (true) when a job is in a non-allowlisted terminal failure state.
# Outputs "<name>\t<conclusion>" on stdout for the first matching job.
find_failed_job() {
  local jobs_json="$1"
  local match
  # Filter: completed conclusion in (failure, cancelled, timed_out, action_required)
  # but skip jobs that are intentionally continue-on-error AND in the allowlist.
  while IFS=$'\t' read -r name conclusion; do
    [[ -z "$name" ]] && continue
    if ! is_allowlisted_job "$name"; then
      echo -e "${name}\t${conclusion}"
      return 0
    fi
  done < <(
    echo "$jobs_json" | jq -r '
      .jobs[]
      | select(.status == "completed")
      | select(.conclusion as $c | ["failure","cancelled","timed_out","action_required"] | index($c))
      | [.name, .conclusion]
      | @tsv
    '
  )
  return 1
}

print_job_transitions() {
  local jobs_json="$1"
  while IFS=$'\t' read -r name state; do
    [[ -z "$name" ]] && continue
    local line="${name}=${state}"
    # Only print if we haven't already printed this exact name=state pair.
    if [[ "$PRINTED_STATES" != *"${line}"* ]]; then
      echo "  · ${state}: ${name}"
      PRINTED_STATES="${PRINTED_STATES}${line}"$'\n'
    fi
  done < <(
    echo "$jobs_json" | jq -r '
      .jobs[]
      | select(.status == "completed")
      | [.name, .conclusion]
      | @tsv
    '
  )
}

cancel_run() {
  local run_id="$1"
  echo "🛑 Cancelling run ${run_id} to fail fast..." >&2
  gh run cancel --repo "$REPO" "$run_id" >/dev/null 2>&1 || true
  # Wait briefly for cancellation to register so the final state reflects it.
  local cancel_start
  cancel_start=$(date +%s)
  while true; do
    local now elapsed
    now=$(date +%s)
    elapsed=$((now - cancel_start))
    if (( elapsed >= 60 )); then
      break
    fi
    if RUN_JSON=$(gh_run_list 2>/dev/null); then
      local s
      s=$(echo "$RUN_JSON" | jq -r '.[0].status // ""')
      if [[ "$s" == "completed" ]]; then
        break
      fi
    fi
    sleep 3
  done
}

while true; do
  NOW=$(date +%s)
  ELAPSED=$((NOW - START_TIME))
  if (( ELAPSED >= MAX_WAIT )); then
    echo "⏱  Timed out after ${ELAPSED}s waiting for workflow to complete."
    echo "   Check: https://github.com/${REPO}/actions"
    exit 2
  fi

  if ! RUN_JSON=$(gh_run_list 2>&1); then
    GH_API_FAILURES=$((GH_API_FAILURES + 1))
    echo "ERROR: gh run list failed (attempt ${GH_API_FAILURES}/${MAX_GH_API_FAILURES}):" >&2
    echo "$RUN_JSON" >&2
    if (( GH_API_FAILURES >= MAX_GH_API_FAILURES )); then
      exit 1
    fi
    sleep "$INTERVAL"
    continue
  fi
  GH_API_FAILURES=0

  STATUS=$(echo "$RUN_JSON" | jq -r '.[0].status // "not_found"')
  CONCLUSION=$(echo "$RUN_JSON" | jq -r '.[0].conclusion // ""')
  RUN_ID=$(echo "$RUN_JSON" | jq -r '.[0].databaseId // ""')

  if [ "$STATUS" = "not_found" ]; then
    echo "  [+${ELAPSED}s] Workflow not started yet..."
    sleep "$INTERVAL"
    continue
  fi

  if [ "$STATUS" = "completed" ]; then
    if [ "$CONCLUSION" = "success" ]; then
      echo "✅ Release workflow succeeded (run ${RUN_ID})"
      echo "   https://github.com/${REPO}/actions/runs/${RUN_ID}"
      exit 0
    else
      echo "❌ Release workflow failed: conclusion=${CONCLUSION} (run ${RUN_ID})"
      echo "   https://github.com/${REPO}/actions/runs/${RUN_ID}"
      exit 1
    fi
  fi

  # Workflow is in_progress / queued / requested. Pull per-job state for
  # progress + fail-fast detection.
  if JOBS_JSON=$(gh_run_jobs "$RUN_ID" 2>/dev/null); then
    print_job_transitions "$JOBS_JSON"
    if (( FAIL_FAST == 1 )); then
      if FAILED_LINE=$(find_failed_job "$JOBS_JSON"); then
        FAILED_NAME=$(echo -e "$FAILED_LINE" | cut -f1)
        FAILED_CONC=$(echo -e "$FAILED_LINE" | cut -f2)
        echo "❌ Job failed: ${FAILED_NAME} (conclusion=${FAILED_CONC})"
        echo "   Cancelling remaining run; downstream publish jobs will not proceed."
        cancel_run "$RUN_ID"
        echo "   https://github.com/${REPO}/actions/runs/${RUN_ID}"
        exit 1
      fi
    fi
  fi

  echo "  [+${ELAPSED}s] Status: ${STATUS} (run ${RUN_ID})"
  sleep "$INTERVAL"
done
