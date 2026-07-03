#!/usr/bin/env bash
#
# Select a corpus of real .org files for the round-trip fuzz harness
# (tests/fuzz_roundtrip.rs) and copy them to a scratch directory.
#
# ORIGINALS ARE NEVER OPENED FOR WRITING: this script only reads them
# (via `cp`). The fuzz harness operates exclusively on the copies under
# the scratch dir.
#
# Selection strategy: from all candidate .org files larger than 20k,
# take the 10 largest plus 10 random others (deduplicated), for a
# corpus biased toward big, structurally rich files.
#
# Usage:
#   scripts/fuzz-corpus.sh [DEST_DIR] [COUNT]
#
# Defaults: DEST_DIR=/tmp/org-cli-fuzz, COUNT=20
# Search roots: ORG_FUZZ_ROOTS, a colon-separated list of directories
# (default: $HOME).
#
# Then run the harness:
#   ORG_FUZZ_DIR=/tmp/org-cli-fuzz cargo test --test fuzz_roundtrip -- --ignored --nocapture

set -euo pipefail

DEST="${1:-/tmp/org-cli-fuzz}"
COUNT="${2:-20}"

IFS=: read -r -a SEARCH_ROOTS <<<"${ORG_FUZZ_ROOTS:-$HOME}"

# Collect candidates: *.org, regular files, > 20k.
mapfile -t candidates < <(
	find "${SEARCH_ROOTS[@]}" -name '*.org' -type f -size +20k 2>/dev/null
)

if [ "${#candidates[@]}" -eq 0 ]; then
	echo "no candidate .org files found under: ${SEARCH_ROOTS[*]}" >&2
	exit 1
fi

# Rank by size (largest first) to pick the top-N largest.
mapfile -t by_size < <(
	printf '%s\n' "${candidates[@]}" |
		xargs -d '\n' stat --printf '%s\t%n\n' 2>/dev/null |
		sort -rn |
		cut -f2-
)

half=$((COUNT / 2))
rest=$((COUNT - half))

# Top `half` largest files.
declare -a chosen=()
declare -A seen=()
for f in "${by_size[@]:0:half}"; do
	chosen+=("$f")
	seen["$f"]=1
done

# `rest` random files from the remainder, avoiding duplicates.
mapfile -t remainder < <(
	printf '%s\n' "${by_size[@]:half}" | shuf
)
for f in "${remainder[@]}"; do
	[ "${#chosen[@]}" -ge "$COUNT" ] && break
	if [ -z "${seen["$f"]:-}" ]; then
		chosen+=("$f")
		seen["$f"]=1
	fi
done

# Prepare a clean destination.
rm -rf "$DEST"
mkdir -p "$DEST"

# Copy with flattened, indexed names: NNN-basename.org
i=0
for f in "${chosen[@]}"; do
	printf -v idx '%03d' "$i"
	base="$(basename "$f")"
	cp -- "$f" "$DEST/${idx}-${base}"
	i=$((i + 1))
done

echo "copied ${#chosen[@]} file(s) to $DEST"
