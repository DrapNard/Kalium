"""Import Kalium PNG exports into Blender Principled materials and verify them.

Run with:
  blender --background --python scripts/qa/blender_import_smoke.py -- \
    /tmp/kalium-export/001_Default examples/TwoMaterials.obj /tmp/kalium.blend

For per-material Texture Set exports, pass a JSON object mapping OBJ material
slot names to export-set folders beneath EXPORT_ROOT, for example:
  blender --background --python scripts/qa/blender_import_smoke.py -- \
    /tmp/kalium-export-all /tmp/slot-set-map.json \
    examples/TwoMaterials.obj /tmp/kalium-sets.blend

This verifies Blender image loading, channel wiring, mesh/material assignment,
.blend save/reopen, a deterministic UV/Base Color pass, and deterministic CPU
Cycles response from the reopened Principled materials. It is a downstream
import smoke, not a production render-quality test.
"""

import bpy
import json
import os
import sys
import math
import tempfile
from pathlib import Path


def args_after_separator():
    if "--" not in sys.argv:
        raise RuntimeError("Expected export directory, mesh path, and blend output after --")
    args = sys.argv[sys.argv.index("--") + 1 :]
    if len(args) not in (3, 4):
        raise RuntimeError(
            "Expected: EXPORT_SET_DIR MESH.obj OUTPUT.blend, or "
            "EXPORT_ROOT SLOT_SET_MAP.json MESH.obj OUTPUT.blend"
        )
    if len(args) == 3:
        paths = [Path(value).resolve() for value in args]
        return (*paths, None)
    export_root = Path(args[0]).resolve()
    manifest = Path(args[1]).resolve()
    mesh_path = Path(args[2]).resolve()
    blend_path = Path(args[3]).resolve()
    return export_root, mesh_path, blend_path, manifest


def load_slot_manifest(export_root, manifest_path):
    try:
        data = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"Cannot read slot→export-folder manifest {manifest_path}: {error}")
    if not isinstance(data, dict) or not data:
        raise RuntimeError("Slot manifest must be a non-empty JSON object")

    mapping = {}
    root = export_root.resolve()
    for slot_name, relative_folder in data.items():
        if not isinstance(slot_name, str) or not slot_name.strip():
            raise RuntimeError("Slot manifest keys must be non-empty material slot names")
        if not isinstance(relative_folder, str) or not relative_folder:
            raise RuntimeError(f"Export folder for slot {slot_name!r} must be a relative path")
        folder = Path(relative_folder)
        if folder.is_absolute():
            raise RuntimeError(f"Export folder for slot {slot_name!r} must be relative to EXPORT_ROOT")
        resolved = (root / folder).resolve()
        if resolved == root or root not in resolved.parents:
            raise RuntimeError(f"Export folder for slot {slot_name!r} escapes EXPORT_ROOT")
        if not resolved.is_dir():
            raise RuntimeError(f"Export folder for slot {slot_name!r} does not exist: {resolved}")
        mapping[slot_name] = resolved
    if len(set(mapping.values())) != len(mapping):
        raise RuntimeError("Each material slot must map to a distinct export folder")
    return mapping


def require_image(directory, filename, color_space):
    path = directory / filename
    if not path.is_file():
        raise RuntimeError(f"Missing exported image: {path}")
    image = bpy.data.images.load(str(path), check_existing=False)
    image.colorspace_settings.name = color_space
    if image.size[0] != image.size[1] or image.size[0] < 64:
        raise RuntimeError(f"Unexpected texture dimensions for {path}: {tuple(image.size)}")
    return image


def make_material(directory, name):
    base = require_image(directory, "Kalium_BaseColor.png", "sRGB")
    orm = require_image(directory, "Kalium_ORM_Unreal_Godot.png", "Non-Color")
    normal = require_image(directory, "Kalium_Normal.png", "Non-Color")
    emissive = require_image(directory, "Kalium_Emissive.png", "sRGB")
    if not (base.size[:] == orm.size[:] == normal.size[:] == emissive.size[:]):
        raise RuntimeError("Exported PBR maps do not share dimensions")

    material = bpy.data.materials.new(name)
    material.use_nodes = True
    nodes = material.node_tree.nodes
    links = material.node_tree.links
    nodes.clear()
    output = nodes.new("ShaderNodeOutputMaterial")
    bsdf = nodes.new("ShaderNodeBsdfPrincipled")
    output.location = (700, 80)
    bsdf.location = (420, 80)
    links.new(bsdf.outputs["BSDF"], output.inputs["Surface"])

    base_node = nodes.new("ShaderNodeTexImage")
    base_node.image = base
    base_node.label = "Kalium Base Color (sRGB)"
    links.new(base_node.outputs["Color"], bsdf.inputs["Base Color"])

    orm_node = nodes.new("ShaderNodeTexImage")
    orm_node.image = orm
    orm_node.label = "Kalium ORM (Non-Color: R=AO, G=Roughness, B=Metallic)"
    separate = nodes.new("ShaderNodeSeparateColor")
    separate.mode = "RGB"
    separate.label = "Kalium ORM channel swizzle"
    links.new(orm_node.outputs["Color"], separate.inputs["Color"])
    links.new(separate.outputs["Green"], bsdf.inputs["Roughness"])
    links.new(separate.outputs["Blue"], bsdf.inputs["Metallic"])

    normal_node = nodes.new("ShaderNodeTexImage")
    normal_node.image = normal
    normal_node.label = "Kalium tangent normal (Non-Color)"
    normal_map = nodes.new("ShaderNodeNormalMap")
    links.new(normal_node.outputs["Color"], normal_map.inputs["Color"])
    links.new(normal_map.outputs["Normal"], bsdf.inputs["Normal"])

    emission_node = nodes.new("ShaderNodeTexImage")
    emission_node.image = emissive
    emission_node.label = "Kalium Emissive (sRGB)"
    links.new(emission_node.outputs["Color"], bsdf.inputs["Emission Color"])

    # Blender's Principled shader has no AO socket. Keep AO available as the
    # red channel for the target engine/material graph rather than inventing
    # a nonstandard connection here.
    material["kalium_ao_source"] = "ORM.R (Non-Color); apply to indirect lighting in target renderer"
    material["kalium_orm_layout"] = "R=AO,G=Roughness,B=Metallic"
    return material


def validate_material(material, expected_directory):
    if not material.use_nodes:
        raise RuntimeError(f"Material {material.name!r} lost its node tree")
    expected = {
        "Kalium_BaseColor.png",
        "Kalium_ORM_Unreal_Godot.png",
        "Kalium_Normal.png",
        "Kalium_Emissive.png",
    }
    expected_color_spaces = {
        "Kalium_BaseColor.png": "sRGB",
        "Kalium_ORM_Unreal_Godot.png": "Non-Color",
        "Kalium_Normal.png": "Non-Color",
        "Kalium_Emissive.png": "sRGB",
    }
    nodes = material.node_tree.nodes
    links = material.node_tree.links

    def image_node(filename):
        candidates = [
            node for node in nodes
            if node.type == "TEX_IMAGE" and node.image
            and Path(bpy.path.abspath(node.image.filepath)).name == filename
        ]
        if len(candidates) != 1:
            raise RuntimeError(
                f"Material {material.name!r} should have one image node for {filename}, "
                f"found {len(candidates)}"
            )
        node = candidates[0]
        actual_path = Path(bpy.path.abspath(node.image.filepath)).resolve()
        expected_path = (expected_directory / filename).resolve()
        if actual_path != expected_path:
            raise RuntimeError(
                f"Material {material.name!r} uses {actual_path} for {filename}; "
                f"expected {expected_path}"
            )
        expected_color_space = expected_color_spaces[filename]
        actual_color_space = node.image.colorspace_settings.name
        if actual_color_space != expected_color_space:
            raise RuntimeError(
                f"Material {material.name!r} uses {actual_color_space} for {filename}; "
                f"expected {expected_color_space} after reopening the .blend"
            )
        return node

    def linked(source, source_socket, destination, destination_socket):
        return any(
            link.from_node == source and link.from_socket.name == source_socket
            and link.to_node == destination and link.to_socket.name == destination_socket
            for link in links
        )

    bsdf = next(node for node in nodes if node.type == "BSDF_PRINCIPLED")
    orm_split = next(node for node in nodes if node.bl_idname == "ShaderNodeSeparateColor")
    normal_map = next(node for node in nodes if node.type == "NORMAL_MAP")
    checks = [
        linked(image_node("Kalium_BaseColor.png"), "Color", bsdf, "Base Color"),
        linked(image_node("Kalium_ORM_Unreal_Godot.png"), "Color", orm_split, "Color"),
        linked(orm_split, "Green", bsdf, "Roughness"),
        linked(orm_split, "Blue", bsdf, "Metallic"),
        linked(image_node("Kalium_Normal.png"), "Color", normal_map, "Color"),
        linked(normal_map, "Normal", bsdf, "Normal"),
        linked(image_node("Kalium_Emissive.png"), "Color", bsdf, "Emission Color"),
    ]
    if not all(checks):
        raise RuntimeError(f"Material {material.name!r} has missing or changed PBR channel links")
    return expected


def configure_render(meshes):
    scene = bpy.context.scene
    # Blender's OBJ importer maps the fixture into the XZ plane. This camera
    # faces along Y so it sees the full surface and gives stable screen-space
    # samples on the left and right material slots.
    camera_data = bpy.data.cameras.new("Kalium Smoke Camera")
    camera = bpy.data.objects.new("Kalium Smoke Camera", camera_data)
    scene.collection.objects.link(camera)
    camera_data.type = "ORTHO"
    camera_data.ortho_scale = 2.2
    camera.location = (0.0, -4.0, 0.0)
    camera.rotation_euler = (math.pi / 2.0, 0.0, 0.0)
    scene.camera = camera

    scene.render.engine = "CYCLES"
    scene.cycles.device = "CPU"
    scene.cycles.samples = 8
    scene.cycles.use_denoising = False
    scene.cycles.max_bounces = 2
    scene.render.threads_mode = "FIXED"
    scene.render.threads = 1
    scene.render.resolution_x = 128
    scene.render.resolution_y = 128
    scene.render.resolution_percentage = 100
    scene.render.image_settings.file_format = "PNG"
    scene.render.film_transparent = False
    scene.view_settings.view_transform = "Standard"
    scene.view_settings.look = "None"
    scene.view_settings.exposure = 0.0
    scene.view_settings.gamma = 1.0

    world = bpy.data.worlds.new("Kalium Smoke World")
    world.use_nodes = True
    background = world.node_tree.nodes.get("Background")
    background.inputs["Color"].default_value = (0.02, 0.02, 0.02, 1.0)
    background.inputs["Strength"].default_value = 0.15
    scene.world = world

    # Broad, camera-facing area lights give the CPU Cycles pass a stable diffuse
    # and specular response. Their fixed positions and powers make the smoke
    # independent of Blender's startup scene and user preferences.
    for name, location, power, size in (
        ("Kalium Smoke Key", (-1.8, -3.0, 2.2), 420.0, 1.4),
        ("Kalium Smoke Fill", (2.0, -2.5, 0.3), 160.0, 2.0),
    ):
        light_data = bpy.data.lights.new(name, type="AREA")
        light_data.energy = power
        light_data.shape = "DISK"
        light_data.size = size
        light = bpy.data.objects.new(name, light_data)
        scene.collection.objects.link(light)
        light.location = location
        direction = -light.location
        light.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()

    # Feed every exported map from the imported UV layer. The emission pass
    # below isolates Base Color and slot assignment; the later Principled pass
    # evaluates the same UV-mapped ORM and tangent-normal images under light.
    seen = set()
    for obj in meshes:
        for slot in obj.material_slots:
            material = slot.material
            if material is None or material.as_pointer() in seen:
                continue
            seen.add(material.as_pointer())
            nodes = material.node_tree.nodes
            links = material.node_tree.links
            output = next((node for node in nodes if node.type == "OUTPUT_MATERIAL"), None)
            base = next(
                (
                    node for node in nodes
                    if node.type == "TEX_IMAGE" and node.image
                    and Path(bpy.path.abspath(node.image.filepath)).name == "Kalium_BaseColor.png"
                ),
                None,
            )
            if output is None or base is None:
                raise RuntimeError(
                    f"Cannot render imported Base Color for material {material.name!r}: "
                    "missing output or Kalium_BaseColor.png image node"
                )
            emission = nodes.new("ShaderNodeEmission")
            emission.label = "Temporary deterministic Base Color smoke pass"
            texcoord = nodes.new("ShaderNodeTexCoord")
            texcoord.label = "Temporary imported UV smoke coordinates"
            for image_node in (
                node for node in nodes
                if node.type == "TEX_IMAGE" and node.image
            ):
                for vector_link in list(image_node.inputs["Vector"].links):
                    links.remove(vector_link)
                links.new(texcoord.outputs["UV"], image_node.inputs["Vector"])
            links.new(base.outputs["Color"], emission.inputs["Color"])
            for surface_link in list(output.inputs["Surface"].links):
                links.remove(surface_link)
            links.new(emission.outputs["Emission"], output.inputs["Surface"])


def render_region_mean(render, x0, y0, x1, y1):
    width, height = render.size[:]
    if not (0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height):
        raise RuntimeError(f"Invalid render sample region {(x0, y0, x1, y1)} for {width}×{height}")
    pixels = render.pixels
    sums = [0.0, 0.0, 0.0]
    count = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            offset = (y * width + x) * 4
            for channel in range(3):
                sums[channel] += pixels[offset + channel]
            count += 1
    return tuple(value / count for value in sums)


def pixel_region_mae(first, second, region):
    x0, y0, x1, y1 = region
    width, height = first[0], first[1]
    if (width, height) != second[:2]:
        raise RuntimeError("Cannot compare renders with different dimensions")
    if not (0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height):
        raise RuntimeError(f"Invalid render comparison region {region}")
    pixels_a, pixels_b = first[2], second[2]
    total = 0.0
    count = 0
    for y in range(y0, y1):
        for x in range(x0, x1):
            offset = (y * width + x) * 4
            for channel in range(3):
                total += abs(pixels_a[offset + channel] - pixels_b[offset + channel])
                count += 1
    return total / count


def render_pixels(scene, render_path, description):
    scene.render.filepath = str(render_path)
    try:
        result = bpy.ops.render.render(write_still=True)
    except Exception as error:
        raise RuntimeError(f"Blender CPU Cycles {description} render failed: {error}") from error
    if "FINISHED" not in result or not render_path.is_file():
        raise RuntimeError(f"Blender CPU Cycles did not produce {description} render: {result}")
    render = bpy.data.images.load(str(render_path), check_existing=False)
    try:
        if tuple(render.size) != (128, 128):
            raise RuntimeError(f"Expected a 128×128 render, got {tuple(render.size)}")
        pixels = tuple(render.pixels[:])
        if not all(value == value and abs(value) != float("inf") for value in pixels):
            raise RuntimeError(f"CPU Cycles {description} render contains non-finite values")
        return (128, 128, pixels)
    finally:
        bpy.data.images.remove(render)


def restored_principled_outputs(meshes):
    """Put the imported materials back on their Principled BSDF surface path."""
    seen = set()
    for obj in meshes:
        for slot in obj.material_slots:
            material = slot.material
            if material is None or material.as_pointer() in seen:
                continue
            seen.add(material.as_pointer())
            nodes = material.node_tree.nodes
            links = material.node_tree.links
            output = next((node for node in nodes if node.type == "OUTPUT_MATERIAL"), None)
            bsdf = next((node for node in nodes if node.type == "BSDF_PRINCIPLED"), None)
            if output is None or bsdf is None:
                raise RuntimeError(f"Material {material.name!r} lost its Principled BSDF")
            for link in list(output.inputs["Surface"].links):
                links.remove(link)
            links.new(bsdf.outputs["BSDF"], output.inputs["Surface"])


def material_bsdfs(meshes):
    result = []
    seen = set()
    for obj in meshes:
        for slot in obj.material_slots:
            material = slot.material
            if material is None or material.as_pointer() in seen:
                continue
            seen.add(material.as_pointer())
            bsdf = next(
                (node for node in material.node_tree.nodes if node.type == "BSDF_PRINCIPLED"),
                None,
            )
            if bsdf is not None:
                result.append((material, bsdf))
    return result


def render_without_socket_maps(meshes, socket_name, default_value, scene, render_path):
    """Render a counterfactual with one mapped Principled input held constant."""
    changed = []
    for material, bsdf in material_bsdfs(meshes):
        socket = bsdf.inputs[socket_name]
        old_default = tuple(socket.default_value) if hasattr(socket.default_value, "__len__") else socket.default_value
        old_links = [(link.from_socket, link.to_socket) for link in socket.links]
        for link in list(socket.links):
            material.node_tree.links.remove(link)
        socket.default_value = default_value
        changed.append((material, socket, old_default, old_links))
    try:
        return render_pixels(scene, render_path, f"{socket_name} control")
    finally:
        for material, socket, old_default, old_links in changed:
            socket.default_value = old_default
            for source, destination in old_links:
                material.node_tree.links.new(source, destination)


def validate_fixture_pbr_values(slot_mapping):
    """Check the richer two-set acceptance fixture actually exercises all maps."""
    if slot_mapping is None or not {"Leather", "Metal"}.issubset(slot_mapping):
        return

    def center_pixel(material_name, filename):
        material = next(
            material for material in bpy.data.materials
            if material.get("kalium_obj_slot") == material_name
        )
        node = next(
            node for node in material.node_tree.nodes
            if node.type == "TEX_IMAGE" and node.image
            and Path(bpy.path.abspath(node.image.filepath)).name == filename
        )
        image = node.image
        offset = ((image.size[1] // 2) * image.size[0] + image.size[0] // 2) * 4
        return tuple(float(value) for value in image.pixels[offset:offset + 4])

    leather_orm = center_pixel("Leather", "Kalium_ORM_Unreal_Godot.png")
    metal_orm = center_pixel("Metal", "Kalium_ORM_Unreal_Godot.png")
    leather_normal = center_pixel("Leather", "Kalium_Normal.png")
    metal_normal = center_pixel("Metal", "Kalium_Normal.png")
    # The document compositor normalizes authored tangent vectors before
    # export, so compare Metal against the normalized encoding of its fixture
    # input rather than the original byte triplet.
    source_normal = [2.0 * value / 255.0 - 1.0 for value in (180, 128, 250)]
    source_length = math.sqrt(sum(value * value for value in source_normal))
    expected_metal_normal = tuple(
        (value / source_length + 1.0) * 0.5 for value in source_normal
    )
    # The fixture contract assigns distinct roughness and metallic values and a
    # visibly tilted tangent normal to Metal. Check decoded Non-Color image
    # values with tolerance for Blender's float conversion.
    expected = (
        ("Leather ORM roughness", leather_orm[1], 224 / 255),
        ("Metal ORM roughness", metal_orm[1], 32 / 255),
        ("Leather ORM metallic", leather_orm[2], 16 / 255),
        ("Metal ORM metallic", metal_orm[2], 224 / 255),
        ("Metal normal X", metal_normal[0], expected_metal_normal[0]),
        ("Metal normal Y", metal_normal[1], expected_metal_normal[1]),
        ("Metal normal Z", metal_normal[2], expected_metal_normal[2]),
    )
    for label, actual, target in expected:
        if abs(actual - target) > 0.012:
            raise RuntimeError(f"Two-set fixture {label} is {actual:.4f}; expected {target:.4f}")
    if max(abs(a - b) for a, b in zip(leather_normal[:3], metal_normal[:3])) < 0.1:
        raise RuntimeError("Two-set fixture normals do not differ enough to test Normal response")


def validate_pbr_render(meshes, slot_mapping, scene, temp_dir):
    restored_principled_outputs(meshes)
    baseline = render_pixels(scene, Path(temp_dir) / "pbr.png", "Principled PBR")
    regions = []
    if slot_mapping is not None and {"Leather", "Metal"}.issubset(slot_mapping):
        # Avoid the shared material edge and compare the two mapped source areas.
        regions = [(24, 60, 40, 68), (88, 60, 104, 68)]
        leather = render_region_mean_from_pixels(baseline, regions[0])
        metal = render_region_mean_from_pixels(baseline, regions[1])
        separation = sum(abs(a - b) for a, b in zip(leather, metal))
        if separation < 0.08:
            raise RuntimeError(
                "Principled render did not preserve the expected Leather/Metal response: "
                f"Leather={tuple(round(v, 4) for v in leather)}, "
                f"Metal={tuple(round(v, 4) for v in metal)}"
            )
        print(
            "BLENDER_PBR_RENDER_SMOKE_PASS "
            f"Leather={tuple(round(v, 4) for v in leather)} "
            f"Metal={tuple(round(v, 4) for v in metal)}"
        )
    else:
        regions = [(24, 24, 104, 104)]
        center = render_region_mean_from_pixels(baseline, regions[0])
        if max(center) < 0.01:
            raise RuntimeError(f"Principled PBR render is empty: {center}")
        print(f"BLENDER_PBR_RENDER_SMOKE_PASS mean={tuple(round(v, 4) for v in center)}")

    if slot_mapping is not None and {"Leather", "Metal"}.issubset(slot_mapping):
        validate_fixture_pbr_values(slot_mapping)
        # Each counterfactual removes one channel-driven BSDF effect while
        # retaining all other imported map links. The real render must change,
        # proving the exported maps participate in CPU shader evaluation.
        controls = (
            ("Roughness", 0.5, 0.001),
            ("Metallic", 0.0, 0.001),
            ("Normal", (0.0, 0.0, 1.0), 0.0005),
        )
        for socket_name, default_value, threshold in controls:
            control = render_without_socket_maps(
                meshes, socket_name, default_value, scene,
                Path(temp_dir) / f"control-{socket_name.lower()}.png",
            )
            difference = max(pixel_region_mae(baseline, control, region) for region in regions)
            if difference < threshold:
                raise RuntimeError(
                    f"Exported {socket_name} map did not affect the Principled CPU render "
                    f"(counterfactual MAE={difference:.6f}, expected ≥{threshold:.6f})"
                )
            print(f"BLENDER_PBR_MAP_RESPONSE {socket_name}=MAE:{difference:.6f}")


def render_region_mean_from_pixels(render, region):
    x0, y0, x1, y1 = region
    width, height, pixels = render
    if not (0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height):
        raise RuntimeError(f"Invalid render sample region {region}")
    sums = [0.0, 0.0, 0.0]
    count = (x1 - x0) * (y1 - y0)
    for y in range(y0, y1):
        for x in range(x0, x1):
            offset = (y * width + x) * 4
            for channel in range(3):
                sums[channel] += pixels[offset + channel]
    return tuple(value / count for value in sums)


def render_and_validate(meshes, slot_mapping):
    configure_render(meshes)
    scene = bpy.context.scene
    with tempfile.TemporaryDirectory(prefix="kalium-blender-render-") as temp_dir:
        render_path = Path(temp_dir) / "render.png"
        emission = render_pixels(scene, render_path, "emission/UV")
        render = bpy.data.images.load(str(render_path), check_existing=False)
        try:
            if tuple(render.size) != (128, 128):
                raise RuntimeError(
                    f"Expected a 128×128 Blender render, got {tuple(render.size)}"
                )

            center = render_region_mean(render, 60, 60, 68, 68)
            extrema = [float("inf"), float("-inf")]
            for offset in range(0, len(render.pixels), 4):
                for channel in range(3):
                    value = render.pixels[offset + channel]
                    if not (value == value and abs(value) != float("inf")):
                        raise RuntimeError("Rendered Base Color image contains a non-finite RGB value")
                    extrema[0] = min(extrema[0], value)
                    extrema[1] = max(extrema[1], value)
            if extrema[1] - extrema[0] < 0.005 or max(center) < 0.01:
                raise RuntimeError(
                    "Rendered Base Color image is empty or nearly uniform: "
                    f"center RGB={tuple(round(v, 4) for v in center)}, "
                    f"range=({extrema[0]:.4f}, {extrema[1]:.4f})"
                )

            if slot_mapping is not None and {"Leather", "Metal"}.issubset(slot_mapping):
                # The source fixture assigns Leather to the left half and Metal to the
                # right half. Sample away from the shared edge so antialiasing cannot
                # turn the material-slot distinction into a boundary test.
                leather = render_region_mean(render, 24, 60, 40, 68)
                metal = render_region_mean(render, 88, 60, 104, 68)
                separation = sum(abs(a - b) for a, b in zip(leather, metal))
                if separation < 0.2 or leather[0] <= leather[2] or metal[2] <= metal[0]:
                    raise RuntimeError(
                        "Rendered material slots do not show the expected distinct exported "
                        "Base Color response (Leather red on left, Metal blue on right): "
                        f"Leather RGB={tuple(round(v, 4) for v in leather)}, "
                        f"Metal RGB={tuple(round(v, 4) for v in metal)}, "
                        f"separation={separation:.4f}"
                    )
                print(
                    "BLENDER_RENDER_SMOKE_PASS "
                    f"Leather={tuple(round(v, 4) for v in leather)} "
                    f"Metal={tuple(round(v, 4) for v in metal)}"
                )
            else:
                print(
                    "BLENDER_RENDER_SMOKE_PASS "
                    f"center={tuple(round(v, 4) for v in center)} "
                    f"range=({extrema[0]:.4f}, {extrema[1]:.4f})"
                )
        finally:
            bpy.data.images.remove(render)
        validate_pbr_render(meshes, slot_mapping, scene, temp_dir)


def validate_saved_blend(blend_path, slot_mapping=None):
    bpy.ops.wm.open_mainfile(filepath=str(blend_path))
    meshes = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
    if not meshes:
        raise RuntimeError("Reopened Blender file has no imported mesh")

    if slot_mapping is None:
        assigned = [slot.material for obj in meshes for slot in obj.material_slots if slot.material]
        if not assigned:
            raise RuntimeError("Reopened mesh has no assigned material")
        images = validate_material(
            assigned[0], Path(assigned[0]["kalium_export_directory"])
        )
        print(
            f"BLENDER_IMPORT_SMOKE_PASS meshes={len(meshes)} "
            f"materials={len(assigned)} images={len(images)}"
        )
        render_and_validate(meshes, None)
        return

    found_slots = {}
    for obj in meshes:
        if "kalium_obj_slots" not in obj:
            raise RuntimeError(f"Reopened mesh {obj.name!r} lost its original OBJ slot names")
        slot_names = json.loads(obj["kalium_obj_slots"])
        if len(slot_names) != len(obj.material_slots):
            raise RuntimeError(f"Reopened mesh {obj.name!r} changed its material slot count")
        for slot_name, slot in zip(slot_names, obj.material_slots):
            if slot_name not in slot_mapping:
                raise RuntimeError(f"Reopened mesh has unmapped OBJ material slot {slot_name!r}")
            if slot.material is None or slot.material.get("kalium_obj_slot") != slot_name:
                raise RuntimeError(
                    f"Reopened OBJ slot {slot_name!r} does not use its mapped Kalium material"
                )
            found_slots.setdefault(slot_name, []).append(slot.material)
    if set(found_slots) != set(slot_mapping):
        raise RuntimeError(
            f"Reopened material-slot mapping differs: expected {sorted(slot_mapping)}, "
            f"found {sorted(found_slots)}"
        )

    for slot_name, directory in slot_mapping.items():
        materials = found_slots[slot_name]
        for material in materials:
            if material.get("kalium_obj_slot") != slot_name:
                raise RuntimeError(f"Material assigned to slot {slot_name!r} has a mismatched tag")
            validate_material(material, directory)

    # All four checked maps for a slot must resolve under that slot's distinct set directory.
    base_paths = {
        slot_name: (directory / "Kalium_BaseColor.png").resolve()
        for slot_name, directory in slot_mapping.items()
    }
    if len(set(base_paths.values())) != len(base_paths):
        raise RuntimeError("Reopened material slots do not retain distinct Base Color image paths")
    print(
        f"BLENDER_IMPORT_SMOKE_PASS meshes={len(meshes)} "
        f"material_slots={len(found_slots)} per_slot_sets=verified"
    )
    render_and_validate(meshes, slot_mapping)


def assign_slot_materials(meshes, slot_mapping):
    found_names = set()
    materials = {}
    for obj in meshes:
        obj["kalium_obj_slots"] = json.dumps(
            [slot.material.name if slot.material else "" for slot in obj.material_slots]
        )
        for slot in obj.material_slots:
            slot_name = slot.material.name if slot.material else ""
            if not slot_name:
                raise RuntimeError(f"Mesh {obj.name!r} has an unnamed material slot")
            found_names.add(slot_name)
    if found_names != set(slot_mapping):
        raise RuntimeError(
            f"Imported OBJ material slots differ from manifest: expected {sorted(slot_mapping)}, "
            f"found {sorted(found_names)}"
        )

    for slot_name, directory in slot_mapping.items():
        material = make_material(directory, f"Kalium exported PBR {slot_name}")
        material["kalium_obj_slot"] = slot_name
        material["kalium_export_directory"] = str(directory)
        materials[slot_name] = material

    for obj in meshes:
        for slot in obj.material_slots:
            slot_name = slot.material.name
            slot.material = materials[slot_name]


def main():
    export_dir, mesh_path, blend_path, manifest_path = args_after_separator()
    if not export_dir.is_dir() or not mesh_path.is_file():
        raise RuntimeError("Export directory or mesh file does not exist")
    slot_mapping = (
        load_slot_manifest(export_dir, manifest_path) if manifest_path is not None else None
    )
    bpy.ops.wm.read_factory_settings(use_empty=True)
    bpy.ops.wm.obj_import(filepath=str(mesh_path))
    meshes = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
    if not meshes:
        raise RuntimeError("Blender did not import a mesh")
    if slot_mapping is None:
        material = make_material(export_dir, "Kalium exported PBR")
        material["kalium_export_directory"] = str(export_dir)
        for obj in meshes:
            if not obj.data.materials:
                obj.data.materials.append(material)
            else:
                for index in range(len(obj.data.materials)):
                    obj.data.materials[index] = material
    else:
        assign_slot_materials(meshes, slot_mapping)
    blend_path.parent.mkdir(parents=True, exist_ok=True)
    bpy.ops.wm.save_as_mainfile(filepath=str(blend_path))
    validate_saved_blend(blend_path, slot_mapping)
    # Blender 5.2's headless extension shutdown tries to mutate the host
    # installation's read-only extension cache in restricted environments.
    # End this dedicated smoke-test process after all saved-file checks pass.
    sys.stdout.flush()
    os._exit(0)


if __name__ == "__main__":
    main()
