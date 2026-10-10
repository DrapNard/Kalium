# Experimental GPU compute painting (DrapNard)

**Status:** v0.2.0-alpha.2 — source prototype, **not hardware validated**.

The GUI currently uses the deterministic CPU rasterizer. The optional `gpu-experimental` Cargo feature adds a minimal compute-only stamp path using wgpu 24 and a WGSL shader. It is a reference for a future real-time implementation, **not** a working GPU viewport or a complete Substance Painter paint engine.

## Building and running

```sh
cargo check --all-targets --features gpu-experimental
cargo run --features gpu-experimental -- gpu-stamp input.png output.png 0.5 0.5 24
```

The command loads a PNG/JPEG RGBA image, dispatches one Paint stamp on the available GPU, reads pixels back and saves a PNG. An adapter and compute-capable graphics driver are required. `gpu-stamp` does not appear when compiled without the Cargo feature.

## Pipeline

1. Pack input RGBA8 as one little-endian `u32` per texel.
2. Upload to a read-write storage buffer and upload a 48-byte uniform record.
3. Dispatch the compute shader with one thread per pixel (`8×8` workgroups).
4. Use the CPU brush formula: hardness/smoothstep feather, opacity and straight-alpha `source-over`; erase affects alpha only.
5. Copy the output buffer to a CPU-readable buffer and reconstruct RGBA8.

This first prototype deliberately does not share the GUI's wgpu `Device`, texture atlas, document state or sparse history. It supports **Paint/Erase** only, **one** stamp per invocation; other brush modes are rejected.

## Validation requirements before interactive integration

- [ ] Verify `cargo check --all-targets --features gpu-experimental` on Linux, macOS and Windows.
- [ ] Run on real Vulkan, Metal and D3D12 adapters and reject absent adapters cleanly.
- [ ] Compare output against CPU `Brush::stamp` with tolerance `<= 1` per channel for representative radius/hardness/opacity cases.
- [ ] Add GPU error scopes and shader validation tests.
- [ ] Convert to long-lived, tiled GPU-resident textures with persistent pipeline/device reuse.
- [ ] Integrate mask/UV clipping, Undo/Redo deltas, multi-channel painting and brush presets.
- [ ] GPU-side picking and 3D projection / tangent-space PBR shading.
- [ ] Test behavior for 4K maps, overlapping UVs, seam boundaries, null/invalid GPU drivers and device loss.

A failed test is a blocker for shipping the GPU path as the default. The CPU reference remains authoritative until measured parity is established.
