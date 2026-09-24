#!/bin/sh
# SessionStart hook (matcher compact|clear) of the claude-dagq plugin.
#
# Only the sessions `dagq up` opens are affected: DAGQ_ROLE (inbox or
# planner, which `up` puts in the workspace's environment with
# --env) selects `dagq status --role <role>` (supervisors, unfinished runs,
# the attention and asks addressed to that role, and the next cursor) on
# stdout, which Claude Code adds to the context, so the session re-orients
# itself after compaction or /clear. The status comes after one line of text
# naming the role and its skill: Claude Code reads a stdout that is JSON as
# the hook's control output and drops its unknown keys, so the bare status
# would never reach the context. Every other session, workers included,
# gets no output. The hook never fails the session start: when status cannot
# be read it prints one line saying why and exits 0. startup is left to each
# session's initial prompt.
case "${DAGQ_ROLE:-}" in
  inbox | planner) role=$DAGQ_ROLE ;;
  *) exit 0 ;;
esac

launcher=$(CDPATH='' cd -- "$(dirname -- "$0")/../bin" && pwd)/dagq

if [ -n "${DAGQ_BIN:-}" ]; then
  if [ ! -x "$DAGQ_BIN" ]; then
    printf 'dagq status unavailable: DAGQ_BIN is not an executable file: %s\n' "$DAGQ_BIN"
    exit 0
  fi
elif ! command -v dagq >/dev/null 2>&1; then
  echo "dagq status unavailable: dagq was not found on PATH and DAGQ_BIN is unset; run the dagq skill's --resolve for the install steps"
  exit 0
fi

# `up` names the session's queue in DAGQ_QUEUE; an explicit DAGQ_DB wins.
if [ -z "${DAGQ_DB:-}" ] && [ -n "${DAGQ_QUEUE:-}" ]; then
  DAGQ_DB=$DAGQ_QUEUE
  export DAGQ_DB
fi

if output=$("$launcher" status --role "$role" 2>/dev/null); then
  printf 'This session is the dagq %s (DAGQ_ROLE=%s); follow the dagq-%s skill of the dagq plugin. The queue status for this role (dagq status --role %s):\n' "$role" "$role" "$role" "$role"
  printf '%s\n' "$output"
else
  # Ask again for the error alone and keep it on one line; printf, because
  # sh's echo expands the `\n` escapes inside the JSON error.
  error=$("$launcher" status --role "$role" 2>&1 >/dev/null | tr -s '\n' ' ')
  printf 'dagq status failed: %s\n' "$error"
fi
exit 0
