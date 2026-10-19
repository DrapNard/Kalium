#!/usr/bin/env python3
"""Create private SPP fixtures through Painter's documented local API.

This is a research/oracle harness, not Kalium's SPP reader or writer. It never
uses an existing Painter process/project, and generated SPPs are not published.
"""

import argparse
import base64
import hashlib
import http.client
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import time


MESH = """# Original Kalium interoperability test quad
o KaliumFixture
v -1 -1 0
v 1 -1 0
v 1 1 0
v -1 1 0
vt 0 0
vt 1 0
vt 1 1
vt 0 1
vn 0 0 1
f 1/1/1 2/2/1 3/3/1
f 1/1/1 3/3/1 4/4/1
"""
MAX_RESPONSE = 64 * 1024
MAX_FIXTURE = 128 * 1024 * 1024


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--painter", required=True, type=Path)
    parser.add_argument("--layer-variants", action="store_true",
                        help="Create and reopen controlled name/opacity/visibility saves")
    args = parser.parse_args()
    if not args.painter.is_file():
        raise RuntimeError("Painter executable does not exist")
    try:
        connection = socket.create_connection(("127.0.0.1", 60041), timeout=1)
    except OSError:
        pass
    else:
        connection.close()
        raise RuntimeError("Existing remote endpoint: refusing to touch its project")

    root = Path(tempfile.mkdtemp(prefix="kalium-spp-fixture-"))
    print(json.dumps({"stage": "fixture_directory", "root": str(root)}), flush=True)
    mesh = root / "quad.obj"
    mesh.write_text(MESH, encoding="utf-8")
    deadline = time.monotonic() + 50
    process = subprocess.Popen(
        [str(args.painter), "--enable-remote-scripting", "--disable-version-checking"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    def remote(script):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Fixture operation deadline exceeded")
        client = http.client.HTTPConnection("127.0.0.1", 60041, timeout=min(5, remaining))
        try:
            client.request(
                "POST", "/run.json",
                json.dumps({"python": base64.b64encode(script.encode()).decode()}),
                {"Content-Type": "application/json"},
            )
            response = client.getresponse()
            body = response.read(MAX_RESPONSE + 1)
            if len(body) > MAX_RESPONSE:
                raise RuntimeError("Oversized API response")
            if response.status != 200:
                raise RuntimeError(f"API status {response.status}")
            value = json.loads(body)
            if isinstance(value, dict) and "error" in value:
                raise RuntimeError(f"Painter API error: {value['error']}")
            return value
        finally:
            client.close()

    def wait_ready(predicate):
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"Owned Painter process exited: {process.returncode}")
            try:
                if remote(predicate):
                    return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.25)
        raise TimeoutError("Painter did not become ready")

    def inspect_file(path):
        digest = hashlib.sha256()
        count = 0
        with path.open("rb") as stream:
            header = stream.read(64)
            stream.seek(0)
            while chunk := stream.read(64 * 1024):
                count += len(chunk)
                if count > MAX_FIXTURE:
                    raise RuntimeError("Fixture exceeds research byte budget")
                digest.update(chunk)
        return {"name": path.name, "bytes": count, "sha256": digest.hexdigest(),
                "header_hex": header.hex()}

    try:
        wait_ready("True")
        if remote("__import__('os').getpid()") != process.pid:
            raise RuntimeError("Remote endpoint is not the owned Painter process")
        remote("import substance_painter")
        if remote("substance_painter.project.is_open()"):
            raise RuntimeError("Painter reopened a project: refusing to replace it")
        api_version = remote("substance_painter.__version__")
        remote(f"substance_painter.project.create(mesh_file_path={str(mesh)!r}, "
               "settings=substance_painter.project.Settings(default_texture_resolution=128))")
        editable = ("substance_painter.project.is_open() and "
                    "not substance_painter.project.is_busy() and "
                    "substance_painter.project.is_in_edition_state()")
        wait_ready(editable)
        baseline = root / "baseline-full.spp"
        copy = root / "same-project-full-copy.spp"
        remote(f"substance_painter.project.save_as({str(baseline)!r}, "
               "substance_painter.project.ProjectSaveMode.Full)")
        identity = remote("str(substance_painter.project.get_uuid())")
        remote(f"substance_painter.project.save_as_copy({str(copy)!r}, "
               "substance_painter.project.ProjectSaveMode.Full)")
        baseline_before_reopen = inspect_file(baseline)
        copy_info = inspect_file(copy)
        remote("substance_painter.project.close()")
        remote(f"substance_painter.project.open({str(baseline)!r})")
        wait_ready(editable)
        reopened_identity = remote("str(substance_painter.project.get_uuid())")
        # The public API does not promise a persistent UUID across open().
        # Record both observations rather than assuming storage semantics.
        if inspect_file(baseline)["sha256"] != baseline_before_reopen["sha256"]:
            raise RuntimeError("Opening the fixture modified the source archive")
        manifest = {
            "purpose": "private clean-room format observation; not Kalium SPP compatibility",
            "api_version": api_version, "project_uuid_at_save": identity,
            "project_uuid_after_reopen": reopened_identity,
            "uuid_stable_across_reopen": identity == reopened_identity,
            "painter_saved_version": remote("substance_painter.project.last_saved_substance_painter_version()"),
            "painter_reopen_editable": True,
            "mesh_sha256": hashlib.sha256(MESH.encode()).hexdigest(),
            "files": [baseline_before_reopen, copy_info],
        }
        if args.layer_variants:
            remote("import substance_painter.layerstack; import substance_painter.textureset")
            node = ("substance_painter.layerstack.get_root_layer_nodes("
                    "substance_painter.textureset.get_active_stack())[0]")
            channel = "substance_painter.textureset.ChannelType.BaseColor"

            def layer_state():
                return remote(f"[{node}.get_name(), {node}.get_opacity({channel}), "
                              f"{node}.is_visible()]")

            initial = layer_state()
            variants = []
            steps = [
                ("renamed-full.spp", f"{node}.set_name('Kalium01')"),
                ("opacity-full.spp", f"{node}.set_opacity(0.375, {channel})"),
                ("hidden-full.spp", f"{node}.set_visible(False)"),
            ]
            for filename, mutation in steps:
                remote(mutation)
                wait_ready(editable)
                path = root / filename
                remote(f"substance_painter.project.save_as_copy({str(path)!r}, "
                       "substance_painter.project.ProjectSaveMode.Full)")
                variants.append({"file": inspect_file(path), "state": layer_state(),
                                 "mode": "Full"})
            incremental = root / "visible-incremental.spp"
            remote(f"substance_painter.project.save_as({str(incremental)!r}, "
                   "substance_painter.project.ProjectSaveMode.Full)")
            remote(f"{node}.set_visible(True)")
            wait_ready(editable)
            remote("substance_painter.project.save("
                   "substance_painter.project.ProjectSaveMode.Incremental)")
            variants.append({"file": inspect_file(incremental), "state": layer_state(),
                             "mode": "Full followed by Incremental"})
            for variant in variants:
                remote("substance_painter.project.close()")
                path = root / variant["file"]["name"]
                remote(f"substance_painter.project.open({str(path)!r})")
                wait_ready(editable)
                if layer_state() != variant["state"]:
                    raise RuntimeError(f"Layer state changed on reopen: {path.name}")
                if inspect_file(path)["sha256"] != variant["file"]["sha256"]:
                    raise RuntimeError(f"Reopen modified archive: {path.name}")
                variant["reopen_state_verified"] = True
            manifest["initial_layer_state"] = initial
            manifest["layer_variants"] = variants
        (root / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
        remote("substance_painter.project.close()")
        print(json.dumps({"root": str(root), **manifest}), flush=True)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
