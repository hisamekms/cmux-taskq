#!/bin/sh
# Check the IDs of the ADRs under docs/adr/ (ADR-t598-1). Two forms exist:
#
# - Four-digit ADRs (NNNN-<slug>.md, 0001 and later): the number is unique and
#   the frontmatter id is adr-NNNN. 0000-template.md is not checked.
# - Task-ID ADRs (<YYYY-MM-DD>-t<task ID>-<N>-<slug>.md): the name has the
#   branch number N (from 1, even for a single ADR), the frontmatter id is
#   adr-t<task ID>-<N>, t<task ID>-<N> is unique, and the date equals the
#   frontmatter accepted_on when the ADR has one (an accepted, superseded or
#   deprecated ADR must have one).
#
# Meant to be run from the repository root (`sh scripts/check-adr-numbers.sh`).
# When run from anywhere else it changes to the repository root found from the
# script's own location, so the result does not depend on the cwd.
#
# Any other .md under docs/adr/ except README.md is reported.
#
# Exit 0 when every ADR is fine, 1 when any check fails (the offending files go
# to stderr), 2 when docs/adr/ is not found.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d docs/adr ]; then
  echo "check-adr-numbers: docs/adr not found under $root" >&2
  exit 2
fi

status=0

# Print the value of a top-level frontmatter field ($2) of file $1.
field() {
  awk -v key="$2" '
    NR == 1 && $0 != "---" { exit }
    NR > 1 && $0 == "---" { exit }
    index($0, key ":") == 1 {
      v = substr($0, length(key) + 2)
      sub(/^[[:space:]]+/, "", v); sub(/[[:space:]]+$/, "", v)
      print v; exit
    }
  ' "$1"
}

old_numbers=""
new_ids=""

for f in docs/adr/*.md; do
  [ -e "$f" ] || continue
  b=$(basename "$f")
  case "$b" in
    README.md | 0000-template.md)
      continue
      ;;
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*)
      parts=$(echo "$b" | sed -n 's/^\([0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}\)-t\([1-9][0-9]*\)-\([1-9][0-9]*\)-[a-z0-9][a-z0-9-]*\.md$/\1 \2 \3/p')
      if [ -z "$parts" ]; then
        echo "check-adr-numbers: $f does not match <YYYY-MM-DD>-t<task ID>-<N>-<slug>.md (the branch number N is required, from 1)" >&2
        status=1
        continue
      fi
      set -- $parts
      date=$1 key="t$2-$3"
      new_ids="$new_ids$key $f
"
      id=$(field "$f" id)
      if [ "$id" != "adr-$key" ]; then
        echo "check-adr-numbers: $f has id '${id:-<missing>}', expected 'adr-$key'" >&2
        status=1
      fi
      accepted_on=$(field "$f" accepted_on)
      st=$(field "$f" status)
      if [ -n "$accepted_on" ]; then
        if [ "$accepted_on" != "$date" ]; then
          echo "check-adr-numbers: $f is dated $date but has accepted_on '$accepted_on'" >&2
          status=1
        fi
      else
        case "$st" in
          accepted | superseded | deprecated)
            echo "check-adr-numbers: $f is $st but has no accepted_on to match its date $date" >&2
            status=1
            ;;
        esac
      fi
      ;;
    [0-9][0-9][0-9][0-9]-*)
      n=$(echo "$b" | cut -c1-4)
      old_numbers="$old_numbers$n $f
"
      id=$(field "$f" id)
      if [ "$id" != "adr-$n" ]; then
        echo "check-adr-numbers: $f has id '${id:-<missing>}', expected 'adr-$n'" >&2
        status=1
      fi
      ;;
    *)
      echo "check-adr-numbers: $f matches neither NNNN-<slug>.md nor <YYYY-MM-DD>-t<task ID>-<N>-<slug>.md" >&2
      status=1
      ;;
  esac
done

# Duplicate four-digit numbers and duplicate task-ID ADR IDs.
report_dups() {
  label=$1 list=$2
  dups=$(printf '%s' "$list" | awk 'NF { print $1 }' | sort | uniq -d)
  for d in $dups; do
    echo "check-adr-numbers: $label $d is used by more than one file:" >&2
    printf '%s' "$list" | awk -v d="$d" '$1 == d { print "  " $2 }' >&2
    status=1
  done
}
report_dups "ADR number" "$old_numbers"
report_dups "ADR ID" "$new_ids"

exit "$status"
