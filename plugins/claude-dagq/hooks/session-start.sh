#!/bin/sh
# SessionStart hook (matcher compact|clear) of the claude-dagq plugin.
#
# Only a maintainer session (DAGQ_ROLE=maintainer, set by `dagq up`) is
# affected: it gets `dagq status` (supervisors, unfinished runs, attention and
# the next cursor) on stdout, which Claude Code adds to the context, so the
# session re-orients itself after compaction or /clear. Every other session,
# workers included, gets no output. The hook never fails the session start:
# when status cannot be read it prints one line saying why and exits 0.
# startup is left to the maintainer's initial prompt.
[ "${DAGQ_ROLE:-}" = maintainer ] || exit 0

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

# `up` names the maintainer's queue in DAGQ_QUEUE; an explicit DAGQ_DB wins.
if [ -z "${DAGQ_DB:-}" ] && [ -n "${DAGQ_QUEUE:-}" ]; then
  DAGQ_DB=$DAGQ_QUEUE
  export DAGQ_DB
fi

if output=$("$launcher" status 2>/dev/null); then
  printf '%s\n' "$output"
else
  # Ask again for the error alone and keep it on one line; printf, because
  # sh's echo expands the `\n` escapes inside the JSON error.
  error=$("$launcher" status 2>&1 >/dev/null | tr -s '\n' ' ')
  printf 'dagq status failed: %s\n' "$error"
fi
exit 0
