#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 <kalium-binary>" >&2
    exit 2
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
KALIUM_BIN=$1
PROJECT=$REPO_ROOT/examples/Kalium-Demo.kalium

if [ ! -x "$KALIUM_BIN" ]; then
    echo "Kalium binary is missing or not executable: $KALIUM_BIN" >&2
    exit 2
fi
if [ ! -f "$PROJECT" ]; then
    echo "Project file does not exist: $PROJECT" >&2
    exit 2
fi

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kalium-cli-smoke.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM

python3 - "$PROJECT" >"$WORK_DIR/source-before.sha256" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
with open(sys.argv[1], "rb") as source:
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY

"$KALIUM_BIN" validate "$PROJECT"
"$KALIUM_BIN" inspect "$PROJECT" >"$WORK_DIR/inspect.txt"
"$KALIUM_BIN" preview "$PROJECT" "$WORK_DIR/preview.png"
"$KALIUM_BIN" uv-mask "$PROJECT" "$WORK_DIR/uv-mask.png"
"$KALIUM_BIN" bake-ao "$PROJECT" "$WORK_DIR/ambient-occlusion.png"
"$KALIUM_BIN" bake-mesh-maps "$PROJECT" "$WORK_DIR/mesh-maps"
"$KALIUM_BIN" bake-material-id "$PROJECT" "$WORK_DIR/material-id.png"
"$KALIUM_BIN" export-all "$PROJECT" "$WORK_DIR/exports"
"$KALIUM_BIN" export-all-godot "$PROJECT" "$WORK_DIR/godot-exports"
"$KALIUM_BIN" export-all-unity "$PROJECT" "$WORK_DIR/unity-exports"
"$KALIUM_BIN" export-all-gltf "$PROJECT" "$WORK_DIR/gltf-exports"

python3 - "$PROJECT" "$WORK_DIR" <<'PY'
import hashlib
import json
import os
import struct
import sys

project, work = sys.argv[1:]

def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def png_size(path):
    with open(path, "rb") as image:
        header = image.read(24)
    if len(header) != 24 or header[:8] != b"\x89PNG\r\n\x1a\n" or header[12:16] != b"IHDR":
        raise SystemExit(f"Invalid or truncated PNG: {path}")
    width, height = struct.unpack(">II", header[16:24])
    if width == 0 or height == 0:
        raise SystemExit(f"PNG has an empty dimension: {path}")
    return width, height

inspect_path = os.path.join(work, "inspect.txt")
with open(inspect_path, encoding="utf-8") as report:
    summary = report.read()
if "Texture sets: 1" not in summary or "Texture: 512×512" not in summary:
    raise SystemExit("Demo project summary did not contain the expected one-set 512×512 scene")

for name, expected in (("preview.png", (600, 600)),
                       ("uv-mask.png", (512, 512)),
                       ("ambient-occlusion.png", (512, 512)),
                       ("material-id.png", (512, 512))):
    path = os.path.join(work, name)
    actual = png_size(path)
    if actual != expected:
        raise SystemExit(f"Unexpected dimensions for {name}: {actual}, expected {expected}")

material_id_path = os.path.join(work, "material-id.png.json")
with open(material_id_path, encoding="utf-8") as metadata_file:
    material_id = json.load(metadata_file)
encoding = material_id.get("encoding", {})
if (material_id.get("format") != "Kalium Material ID"
        or encoding.get("slot_index_bits") != 24
        or encoding.get("byte_order") != "little-endian"
        or encoding.get("alpha") != {"covered": 255, "uncovered": 0}
        or material_id.get("dimensions") != {"width": 512, "height": 512}
        or not material_id.get("material_slots")):
    raise SystemExit("Material-ID metadata does not match the encoding contract")

mesh_dir = os.path.join(work, "mesh-maps")
metadata_path = os.path.join(mesh_dir, "Kalium_MeshMap_Metadata.json")
with open(metadata_path, encoding="utf-8") as metadata_file:
    metadata = json.load(metadata_file)
if metadata.get("resolution") != 512 or metadata.get("covered_texels", 0) <= 0:
    raise SystemExit("Mesh-map metadata has unexpected resolution or no covered texels")
for name in ("Kalium_WorldNormal.png", "Kalium_Position.png", "Kalium_BakeCoverage.png"):
    if png_size(os.path.join(mesh_dir, name)) != (512, 512):
        raise SystemExit(f"Unexpected mesh-map dimensions: {name}")

export_dir = os.path.join(work, "exports", "001_Default")
export_names = (
    "Kalium_BaseColor.png",
    "Kalium_Roughness.png",
    "Kalium_Metallic.png",
    "Kalium_Normal.png",
    "Kalium_Height.png",
    "Kalium_Emissive.png",
    "Kalium_Opacity.png",
    "Kalium_AmbientOcclusion.png",
    "Kalium_BaseColor_Alpha.png",
    "Kalium_ORM_Unreal_Godot.png",
    "Kalium_MetallicSmoothness_Unity.png",
)
for name in export_names:
    path = os.path.join(export_dir, name)
    if png_size(path) != (512, 512):
        raise SystemExit(f"Missing or incorrectly sized export map: {path}")

godot_dir = os.path.join(work, "godot-exports", "001_Default")
manifest_path = os.path.join(godot_dir, "Kalium_Godot4_Profile.json")
with open(manifest_path, encoding="utf-8") as manifest_file:
    profile = json.load(manifest_file)
if profile.get("target") != "Godot 4" or profile.get("material_type") != "ORMMaterial3D":
    raise SystemExit("Godot export profile has an unexpected engine or material type")
if profile.get("orm_channels") != {"red": "occlusion", "green": "roughness", "blue": "metallic"}:
    raise SystemExit("Godot ORM channel mapping does not match the profile contract")
if profile.get("normal_map_convention") != "OpenGL (+Y)":
    raise SystemExit("Godot profile does not declare the required OpenGL normal convention")
for name in profile["textures"].values():
    if png_size(os.path.join(godot_dir, name)) != (512, 512):
        raise SystemExit(f"Godot profile references a missing or incorrectly sized map: {name}")

unity_dir = os.path.join(work, "unity-exports", "001_Default")
unity_manifest_path = os.path.join(unity_dir, "Kalium_Unity_Profile.json")
with open(unity_manifest_path, encoding="utf-8") as manifest_file:
    unity_profile = json.load(manifest_file)
if unity_profile.get("target") != "Unity" or unity_profile.get("material_workflow") != "metallic":
    raise SystemExit("Unity export profile has an unexpected target or material workflow")
if unity_profile.get("metallic_smoothness_channels") != {
    "red": "metallic", "alpha": "smoothness (1 - roughness)"
}:
    raise SystemExit("Unity metallic-smoothness channel mapping does not match the profile contract")
if unity_profile.get("normal_map_convention") != "OpenGL (+Y)":
    raise SystemExit("Unity profile does not declare its required normal convention")
for name in unity_profile["textures"].values():
    if png_size(os.path.join(unity_dir, name)) != (512, 512):
        raise SystemExit(f"Unity profile references a missing or incorrectly sized map: {name}")

gltf_dir = os.path.join(work, "gltf-exports", "001_Default")
gltf_manifest_path = os.path.join(gltf_dir, "Kalium_glTF_Profile.json")
with open(gltf_manifest_path, encoding="utf-8") as manifest_file:
    gltf_profile = json.load(manifest_file)
if gltf_profile.get("target") != "glTF 2.0" or gltf_profile.get("material_workflow") != "metallic-roughness":
    raise SystemExit("glTF export profile has an unexpected target or material workflow")
if gltf_profile.get("metallic_roughness_channels") != {
    "green": "roughness", "blue": "metallic",
    "red": "ignored by metallic-roughness; used for occlusion"
}:
    raise SystemExit("glTF metallic-roughness channel mapping does not match the profile contract")
if gltf_profile.get("occlusion_channel") != "red" or gltf_profile.get("normal_map_convention") != "OpenGL (+Y)":
    raise SystemExit("glTF profile does not declare the expected occlusion or normal convention")
if gltf_profile["textures"]["metallic_roughness"] != gltf_profile["textures"]["occlusion"]:
    raise SystemExit("glTF profile should permit the packed ORM image to serve both texture roles")
for name in gltf_profile["textures"].values():
    if png_size(os.path.join(gltf_dir, name)) != (512, 512):
        raise SystemExit(f"glTF profile references a missing or incorrectly sized map: {name}")

before_path = os.path.join(work, "source-before.sha256")
with open(before_path, encoding="ascii") as digest_file:
    before = digest_file.read().strip()
if sha256(project) != before:
    raise SystemExit("A read-only CLI workflow changed the source project bytes")
print("CLI release smoke passed: project bytes unchanged; preview, UV, AO, mesh-map, material-ID PNG/sidecar, generic maps, Godot ORM, Unity metallic-smoothness and glTF metallic-roughness profiles validated")
PY
