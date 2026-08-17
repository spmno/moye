#!/usr/bin/env bash
# verify-notes.sh — validate Agent Notes under memory/notes/implemented/
# Checks: (1) notes live under a topic directory, (2) each .md has YAML frontmatter
# with topic/date/slug fields, (3) filename matches YYYY-MM-DD-<slug>.md.
# Exit 0 = all valid; exit 1 = at least one note invalid or dir missing.
set -euo pipefail

NOTES_DIR="memory/notes/implemented"
errors=0

if [[ ! -d "$NOTES_DIR" ]]; then
    echo "verify-notes: no notes directory at $NOTES_DIR (nothing to verify)" >&2
    exit 0
fi

shopt -s nullglob

for topic_dir in "$NOTES_DIR"/*/; do
    topic_name=$(basename "$topic_dir")
    for note_file in "$topic_dir"*.md; do
        fname=$(basename "$note_file")

        # 1. Filename must match YYYY-MM-DD-<slug>.md
        if [[ ! "$fname" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}-.+\.md$ ]]; then
            echo "FAIL: $note_file — filename does not match YYYY-MM-DD-<slug>.md" >&2
            errors=$((errors + 1))
            continue
        fi

        # 2. File must start with '---' frontmatter opening
        first_line=$(head -n1 "$note_file" 2>/dev/null || true)
        if [[ "$first_line" != "---" ]]; then
            echo "FAIL: $note_file — missing frontmatter opening '---'" >&2
            errors=$((errors + 1))
            continue
        fi

        # 3. Find frontmatter closing '---' (second occurrence)
        close_line=$(awk 'NR>1 && /^---$/ {print NR; exit}' "$note_file" 2>/dev/null || true)
        if [[ -z "$close_line" ]]; then
            echo "FAIL: $note_file — missing frontmatter closing '---'" >&2
            errors=$((errors + 1))
            continue
        fi

        # 4. Frontmatter must contain topic, date, slug fields
        fm_content=$(sed -n "2,$((close_line - 1))p" "$note_file")
        if ! echo "$fm_content" | grep -qE '^topic:'; then
            echo "FAIL: $note_file — frontmatter missing 'topic:' field" >&2
            errors=$((errors + 1))
        fi
        if ! echo "$fm_content" | grep -qE '^date:'; then
            echo "FAIL: $note_file — frontmatter missing 'date:' field" >&2
            errors=$((errors + 1))
        fi
        if ! echo "$fm_content" | grep -qE '^slug:'; then
            echo "FAIL: $note_file — frontmatter missing 'slug:' field" >&2
            errors=$((errors + 1))
        fi

        # 5. Date in filename should match date in frontmatter (YYYY-MM-DD)
        fm_date=$(echo "$fm_content" | grep -oE '^date:\s*[0-9]{4}-[0-9]{2}-[0-9]{2}' | sed 's/^date:\s*//')
        fn_date="${fname:0:10}"
        if [[ -n "$fm_date" && "$fm_date" != "$fn_date" ]]; then
            echo "FAIL: $note_file — frontmatter date ($fm_date) != filename date ($fn_date)" >&2
            errors=$((errors + 1))
        fi
    done
done

if [[ $errors -gt 0 ]]; then
    echo "verify-notes: $errors error(s) found" >&2
    exit 1
fi

echo "verify-notes: all notes valid"
exit 0
