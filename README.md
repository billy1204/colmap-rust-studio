# COLMAP Rust Studio

A native Windows front end for local COLMAP photogrammetry. Rust Studio manages projects, reconstruction jobs, cancellation, diagnostics, and point-cloud inspection while COLMAP remains the reconstruction engine. Photos and reconstruction data stay on the local computer.

The original console launcher is still included as `colmap-launcher.exe`, but `colmap-studio.exe` is the primary application.

## Current capabilities

- Native folder pickers for photos, output location, and the COLMAP installation.
- Named projects with exclusive workspace claims, validation, and dense disk-space preflight.
- `Quick Sparse Preview` and `RTX Dense Point Cloud` pipelines.
- Background reconstruction with real COLMAP progress, bounded on-screen logs, and a complete `run.log` on disk.
- Cooperative cancellation followed by a timed force-stop of the owned process tree when necessary.
- Atomically persisted recent projects that can reopen completed sparse or dense outputs.
- Interactive 3D point-cloud preview with colored points, camera frustums, image selection, orbit, pan, zoom, and sparse/dense switching.
- Compatibility checks for COLMAP, NVIDIA GPU/VRAM, driver information, plugins, and available project-volume disk space.
- Defensive readers for COLMAP sparse binary models and ASCII or binary little-endian PLY point clouds.

See [GUI_README.md](GUI_README.md) for operating details and verification results.

## Pipelines

| Pipeline | GPU work | CPU work | Output |
| --- | --- | --- | --- |
| Quick Sparse Preview | SIFT extraction and matching | Mapping and bundle adjustment | `sparse/<model>/points3D.bin` |
| RTX Dense Point Cloud | SIFT extraction, matching, and PatchMatch stereo | Mapping, bundle adjustment, and stereo fusion | `dense/<model>/fused.ply` |

The launcher does not claim a speed advantage over the equivalent COLMAP commands. GPU availability and supported stages depend on the selected COLMAP build.

## Verified configuration

- Windows with an NVIDIA GeForce RTX 5050, driver 617.14, and CUDA driver capability 13.4.
- COLMAP 4.2.0 built with CUDA.
- CUDA execution confirmed for SIFT extraction, SIFT matching, and PatchMatch stereo.
- The 128-photo South Building dense run produced 128 depth maps, 128 normal maps, and an 88,238,408-byte `fused.ply` containing 3,268,080 vertices.
- All 128 photos registered in one sparse model.

COLMAP 4.2.1 or newer is recommended because 4.2.1 fixes CUDA, global-mapper, point-color, and output defects present in 4.2.0.

The tested prebuilt COLMAP package does not contain CUDA/cuDSS-enabled Ceres or the experimental Caspar backend, so bundle adjustment falls back to CPU. GPU bundle adjustment requires a custom COLMAP build with Ceres CUDA/cuDSS support or `CASPAR_ENABLED=ON`.

## Install and run

1. Install a compatible CUDA-enabled COLMAP build separately.
2. Build Rust Studio or download a project release when one is available.
3. Start `colmap-studio.exe`.
4. Choose the photo folder, project name, output parent, and pipeline.
5. Check the COLMAP installation under **Advanced engine settings** if it is not detected automatically.
6. Start reconstruction. Successful output is loaded into the preview automatically.

Source photos and the COLMAP installation are never modified. New runs require a new or empty project folder.

## Build and test

```text
cargo fmt --check
cargo test --features gui --all-targets
cargo clippy --features gui --all-targets -- -D warnings
cargo build --release --features gui --bin colmap-studio
```

GitHub Actions applies the same formatting, testing, Clippy, and release-build gates on Windows for pushes and pull requests. The routine GUI-enabled suite currently contains 56 passing tests. Hardware and real-data integration tests are opt-in because they run COLMAP and create reconstruction output.

Example real-data test configuration:

```powershell
$env:COLMAP_TEST_INSTALL = "$env:USERPROFILE\Desktop\colmap"
$env:COLMAP_TEST_IMAGES = "$env:USERPROFILE\Documents\COLMAP Tests\south-building\south-building\images"
$env:COLMAP_TEST_WORKSPACE = "$env:USERPROFILE\Documents\COLMAP Tests\integration-run"
cargo test --release --features gui --test cli real_sample_reconstruction -- --ignored
```

## Current limitations

- The preview displays point clouds and cameras, not connected meshes or photo textures.
- Recent projects store paths and pipeline metadata; there is no portable project file or automatic resume workflow yet.
- Cancellation preserves partial files when COLMAP exits cooperatively, but Rust Studio does not yet resume those files automatically.
- The preview retains at most 100,000 deterministically sampled points and rejects inputs declaring more than 10 million points.
- COLMAP and its dependencies are installed and licensed separately; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Roadmap

- Portable project manifests and automatic resume of interrupted work.
- Optional GPU bundle adjustment when a compatible Ceres CUDA/cuDSS or Caspar build is detected.
- Mesh generation, texture workflows, and richer source-image inspection.
- Installer, signed releases, and automatic update support.

## Console launcher

The original sparse-only console workflow remains available:

```powershell
& '.\colmap-launcher.exe' --colmap "$env:USERPROFILE\Desktop\colmap" --images "$env:USERPROFILE\Documents\COLMAP Tests\south-building\south-building\images" --workspace "$env:USERPROFILE\Documents\COLMAP Tests\my-new-run"
```

Run `colmap-launcher --help` for its complete usage information.
