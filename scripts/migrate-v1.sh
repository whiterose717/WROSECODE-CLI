#!/usr/bin/env bash
set -euo pipefail
old_dir="${1:-$HOME/.wrosecode}"
for session in "$old_dir"/sessions/*.json; do
  [[ -e "$session" ]] || continue
  wrosecode --import-session "$session"
done
echo "Migration complete. Provider and authentication files remain in $old_dir."
