#!/usr/bin/env bash
# Vérifie qu'aucun fichier YAML d'artifact n'est nommé avec un UUID pur.
# Usage : ./company/scripts/check-artifact-naming.sh [root_dir]
set -euo pipefail
ROOT="${1:-.}"
UUID_PATTERN='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.ya?ml$'
FOUND=0
while IFS= read -r -d '' file; do
	basename=$(basename "$file")
	if echo "$basename" | grep -qE "$UUID_PATTERN"; then
		echo "ERROR: UUID filename forbidden: $file" >&2
		FOUND=1
	fi
done < <(find "$ROOT/company" "$ROOT/projects" \( -name '*.yml' -o -name '*.yaml' \) -print0 2>/dev/null)
if [ "$FOUND" -eq 1 ]; then
	echo "FAIL: rename artifact files to <slug>-<8chars-uuid>.yml format" >&2
	exit 1
fi
echo "OK: all artifact filenames conform to naming convention"
