#!/bin/sh
# Check that the migrations under migrations/ are named NNNN_<name>.sql and
# numbered from 0001 without a gap or a repeat (ADR-0067). build.rs refuses
# the same numbers when it lists the migrations; this names them without a
# build, for CI and a task's verification.
#
# Meant to be run from the repository root (`sh scripts/check-migration-numbers.sh`).
# When run from anywhere else it changes to the repository root found from the
# script's own location, so the result does not depend on the cwd.
#
# Exit 0 when the numbers are fine, 1 when a file is misnamed or a number is
# shared or missing (the offending files go to stderr), 2 when migrations/ is
# not found.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d migrations ]; then
  echo "check-migration-numbers: migrations not found under $root" >&2
  exit 2
fi

status=0

# Misnamed .sql files.
for f in migrations/*.sql; do
  [ -e "$f" ] || continue
  case $(basename "$f") in
    [0-9][0-9][0-9][0-9]_?*.sql) ;;
    *)
      echo "check-migration-numbers: $f is not named NNNN_<name>.sql" >&2
      status=1
      ;;
  esac
done

numbers=$(
  for f in migrations/[0-9][0-9][0-9][0-9]_?*.sql; do
    [ -e "$f" ] || continue
    basename "$f" | cut -c1-4
  done | sort
)

# Duplicate numbers.
for n in $(printf '%s\n' "$numbers" | uniq -d); do
  echo "check-migration-numbers: migration number $n is used by more than one file:" >&2
  for f in migrations/"$n"_*.sql; do
    echo "  $f" >&2
  done
  status=1
done

# Numbers run from 0001 without a gap.
expected=1
for n in $(printf '%s\n' "$numbers" | uniq); do
  value=$(printf '%s' "$n" | sed 's/^0*//')
  value=${value:-0}
  if [ "$value" -ne "$expected" ]; then
    if [ "$value" -lt "$expected" ]; then
      echo "check-migration-numbers: migration number $n comes before 0001" >&2
    else
      echo "check-migration-numbers: migration number $(printf '%04d' "$expected") is missing before $(ls migrations/"$n"_*.sql | head -n 1)" >&2
    fi
    status=1
  fi
  if [ "$value" -ge "$expected" ]; then
    expected=$((value + 1))
  fi
done

exit "$status"
