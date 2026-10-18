"""Import Kalium's PNG export into a Blender Principled material and verify it.

Run with:
  blender --background --python scripts/qa/blender_import_smoke.py -- \
    /tmp/kalium-export/001_Default examples/TwoMaterials.obj /tmp/kalium.blend

This verifies Blender image loading, channel wiring, mesh/material assignment,
and .blend save/reopen. It is a smoke acceptance test, not a render-quality test.
"""

import bpy
import os
import sys
from pathlib import Path


def args_after_separator():
    if "--" not in sys.argv:
        raise RuntimeError("Expected export directory, mesh path, and blend output after --")
    args = sys.argv[sys.argv.index("--") + 1 :]
    if len(args) != 3:
        raise RuntimeError("Expected: EXPORT_SET_DIR MESH.obj OUTPUT.blend")
    return Path(args[0]).resolve(), Path(args[1]).resolve(), Path(args[2]).resolve()


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


def validate_saved_blend(blend_path):
    bpy.ops.wm.open_mainfile(filepath=str(blend_path))
    meshes = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
    if not meshes:
        raise RuntimeError("Reopened Blender file has no imported mesh")
    assigned = [slot.material for obj in meshes for slot in obj.material_slots if slot.material]
    if not assigned:
        raise RuntimeError("Reopened mesh has no assigned material")
    material = assigned[0]
    if not material.use_nodes:
        raise RuntimeError("Reopened material lost its node tree")
    images = {node.image.name for node in material.node_tree.nodes if node.type == "TEX_IMAGE" and node.image}
    expected = {
        "Kalium_BaseColor.png",
        "Kalium_ORM_Unreal_Godot.png",
        "Kalium_Normal.png",
        "Kalium_Emissive.png",
    }
    if not expected.issubset(images):
        raise RuntimeError(f"Reopened material is missing image nodes: {expected - images}")

    nodes = material.node_tree.nodes
    links = material.node_tree.links

    def image_node(filename):
        return next(
            node for node in nodes
            if node.type == "TEX_IMAGE" and node.image
            and Path(bpy.path.abspath(node.image.filepath)).name == filename
        )

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
        raise RuntimeError("Reopened material has missing or changed PBR channel links")
    print(f"BLENDER_IMPORT_SMOKE_PASS meshes={len(meshes)} materials={len(assigned)} images={len(expected)}")


def main():
    export_dir, mesh_path, blend_path = args_after_separator()
    if not export_dir.is_dir() or not mesh_path.is_file():
        raise RuntimeError("Export-set directory or mesh file does not exist")
    bpy.ops.wm.read_factory_settings(use_empty=True)
    bpy.ops.wm.obj_import(filepath=str(mesh_path))
    meshes = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
    if not meshes:
        raise RuntimeError("Blender did not import a mesh")
    material = make_material(export_dir, "Kalium exported PBR")
    for obj in meshes:
        if not obj.data.materials:
            obj.data.materials.append(material)
        else:
            for index in range(len(obj.data.materials)):
                obj.data.materials[index] = material
    blend_path.parent.mkdir(parents=True, exist_ok=True)
    bpy.ops.wm.save_as_mainfile(filepath=str(blend_path))
    validate_saved_blend(blend_path)
    # Blender 5.2's headless extension shutdown tries to mutate the host
    # installation's read-only extension cache in restricted environments.
    # End this dedicated smoke-test process after all saved-file checks pass.
    sys.stdout.flush()
    os._exit(0)


if __name__ == "__main__":
    main()
