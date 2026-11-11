#!/usr/bin/env bash
set -euo pipefail

SOURCE_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
DEST_DIR="${FLEET_PASS_INSTALL_DIR:-$HOME/.local/share/shipyard-steward/pass-runner}"
FILES=(fleet-pass.sh must-review.py review_queue.py operator-findings.py)

mkdir -p "$DEST_DIR"
STAGE="$(mktemp -d "$DEST_DIR/.pass-runner-install.XXXXXX")"
cleanup_stage() {
  for file in "${FILES[@]}"; do
    rm -f "$STAGE/$file"
  done
  rmdir "$STAGE" 2>/dev/null || true
}
trap cleanup_stage EXIT

for file in "${FILES[@]}"; do
  cp "$SOURCE_DIR/$file" "$STAGE/$file"
done
chmod 755 "$STAGE/fleet-pass.sh"
chmod 644 "$STAGE/must-review.py" "$STAGE/review_queue.py" "$STAGE/operator-findings.py"
for file in "${FILES[@]}"; do
  mv "$STAGE/$file" "$DEST_DIR/$file"
done

echo "installed fleet-pass runner in $DEST_DIR"
