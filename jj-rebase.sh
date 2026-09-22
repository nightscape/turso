#!/bin/bash
# jj-imerge: incrementally rebase a branch onto a destination,
# stepping through each intermediate commit to keep conflicts small.
#
# Usage: jj-imerge <feature-branch> <destination>
# Example: jj-imerge my-feature main

set -euo pipefail

FEATURE="${1:?Usage: jj-imerge <feature-bookmark> <destination>}"
DEST="${2:?Usage: jj-imerge <feature-bookmark> <destination>}"

# Find the fork point (merge base)
FORK=$(jj log -r "heads(::\"$FEATURE\" & ::\"$DEST\")" \
    -T 'commit_id' --no-graph | head -1)

if [ -z "$FORK" ]; then
    echo "Error: could not find fork point between $FEATURE and $DEST"
    exit 1
fi

echo "Fork point: $(jj log -r "$FORK" -T 'change_id.short()' --no-graph)"

# Get the list of destination commits since the fork, in chronological order
mapfile -t MAIN_COMMITS < <(
    jj log -r "$FORK..\"$DEST\"" -T 'change_id ++ "\n"' \
        --no-graph --reversed
)

TOTAL=${#MAIN_COMMITS[@]}
echo "Incrementally rebasing $FEATURE through $TOTAL commits to reach $DEST"
echo ""

STEP=0
for commit in "${MAIN_COMMITS[@]}"; do
    STEP=$((STEP + 1))
    SHORT=$(jj log -r "$commit" -T 'change_id.short()' --no-graph)
    DESC=$(jj log -r "$commit" \
        -T 'description.first_line().truncate(60)' --no-graph)
    echo "[$STEP/$TOTAL] Rebasing onto $SHORT: $DESC"

    jj rebase -b "$FEATURE" -d "$commit"

    # Check if any feature commits now have conflicts
    CONFLICTS=$(jj log -r "\"$commit\"..\"$FEATURE\"" \
        -T 'if(conflict, change_id.short() ++ "\n", "")' --no-graph)

    if [ -n "$CONFLICTS" ]; then
        echo ""
        echo "  ⚠ Conflicts detected in:"
        echo "$CONFLICTS" | while read -r c; do
            [ -n "$c" ] && echo "    $c"
        done
        echo ""
        echo "  Resolve them (jj new <change>, edit, jj squash),"
        echo "  then press Enter to continue..."
        read -r
    else
        echo "  ✓ clean"
    fi
done

echo ""
echo "Done! $FEATURE is now rebased onto $DEST."
echo "All conflicts resolved incrementally."
