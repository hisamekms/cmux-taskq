#!/bin/sh
# Check that every ADR under docs/adr/ has a unique four-digit number and that
# its frontmatter id is adr-<that number>. 0000-template.md is not checked.
#
# Meant to be run from the repository root (`sh scripts/check-adr-numbers.sh`).
# When run from anywhere else it changes to the repository root found from the
# script's own location, so the result does not depend on the cwd.
#
# Exit 0 when every ADR is fine, 1 when a number is shared or an id does not
# match (the offending files go to stderr), 2 when docs/adr/ is not found.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d docs/adr ]; then
  echo "check-adr-numbers: docs/adr not found under $root" >&2
  exit 2
fi

status=0

# Duplicate numbers.
dups=$(
  for f in docs/adr/[0-9][0-9][0-9][0-9]-*.md; do
    [ -e "$f" ] || continue
    [ "$f" = docs/adr/0000-template.md ] && continue
    basename "$f" | cut -c1-4
  done | sort | uniq -d
)
for n in $dups; do
  echo "check-adr-numbers: ADR number $n is used by more than one file:" >&2
  for f in docs/adr/"$n"-*.md; do
    echo "  $f" >&2
  done
  status=1
done

# Frontmatter id must be adr-<number>.
for f in docs/adr/[0-9][0-9][0-9][0-9]-*.md; do
  [ -e "$f" ] || continue
  [ "$f" = docs/adr/0000-template.md ] && continue
  n=$(basename "$f" | cut -c1-4)
  id=$(awk '
    NR == 1 && $0 != "---" { exit }
    NR > 1 && $0 == "---" { exit }
    /^id:[[:space:]]*/ { sub(/^id:[[:space:]]*/, ""); sub(/[[:space:]]+$/, ""); print; exit }
  ' "$f")
  if [ "$id" != "adr-$n" ]; then
    echo "check-adr-numbers: $f has id '${id:-<missing>}', expected 'adr-$n'" >&2
    status=1
  fi
done

exit "$status"
