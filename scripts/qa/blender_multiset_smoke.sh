#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 <kalium-binary>" >&2
    exit 2
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
KALIUM_BIN=$1
BLENDER_BIN=${BLENDER_BIN:-blender}

if [ ! -x "$KALIUM_BIN" ]; then
    echo "Kalium binary is missing or not executable: $KALIUM_BIN" >&2
    exit 2
fi

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kalium-blender-multiset.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM
FIXTURE=$WORK_DIR/two-materials.kalium
EXPORTS=$WORK_DIR/exports
MANIFEST=$WORK_DIR/slots.json
BLEND=$WORK_DIR/two-materials.blend

KALIUM_BLENDER_FIXTURE_OUTPUT=$FIXTURE cargo test --locked project::tests::write_blender_two_set_fixture -- --ignored --exact --nocapture
"$KALIUM_BIN" validate "$FIXTURE"
"$KALIUM_BIN" export-all "$FIXTURE" "$EXPORTS"

cat >"$MANIFEST" <<'JSON'
{
  "Leather": "001_Default",
  "Metal": "002_Metal"
}
JSON

"$BLENDER_BIN" --background --factory-startup --python-exit-code 1 \
    --python "$SCRIPT_DIR/blender_import_smoke.py" -- \
    "$EXPORTS" "$MANIFEST" "$REPO_ROOT/examples/TwoMaterials.obj" "$BLEND"

echo "BLENDER_MULTISET_SMOKE_PASS: Leather and Metal use distinct Kalium Texture Sets"
