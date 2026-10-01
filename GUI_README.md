# COLMAP Rust Studio — current GUI

## Open
Double-click `colmap-studio.exe` in your launcher folder (for example,
`%USERPROFILE%\Desktop\COLMAP Rust Launcher`).
The original `colmap-launcher.exe` remains available separately.

## Included
- Native Rust/egui window with photo selection, project naming, output-parent selection, and the COLMAP path under Advanced settings.
- Background sparse reconstruction, bounded on-screen logs and full run.log on disk.
- Explicit `Quick Sparse Preview` and `RTX Dense Point Cloud` pipeline cards. Dense mode uses CUDA SIFT extraction/matching and CUDA PatchMatch stereo on GPU 0, then CPU mapping/fusion. Output is `dense/<model>/fused.ply`.
- Start, Resume, and Cancel controls. Cancel first sends COLMAP a cooperative Ctrl+C so supported stages can preserve partial results, then force-stops the owned process tree after 10 seconds if necessary. Restarting the same project validates its `project.json` identity, reuses valid completed stages, and reruns interrupted work with append-only logs.
- Windows Job Object cleanup and child-process wait on cancellation / window close.
- Native sparse `points3D.bin`, `cameras.bin`, `images.bin`, `rigs.bin`, and `frames.bin` parsing plus dense ASCII/binary-little-endian `fused.ply` loading. The 3D preview includes colored points, selectable camera frustums and registered image names, Sparse/Dense switching, orbit, pan, zoom, size controls, and Reset view.
- The preview retains up to 100,000 deterministically sampled points and rejects malformed/truncated files. Input files exceeding 10 million declared points are not supported by this preview.
- Progress bars are shown only when COLMAP emits an explicit current/total counter; no percentages or speedup claims are invented. Engine remains COLMAP.
- Named real stages, dense disk-space preflight, completion metrics/actions, hidden-by-default technical logs, versioned atomic project manifests, recent-project settings, and a compatibility check for COLMAP, plugins, NVIDIA GPU/VRAM, driver, and project-volume disk space.
- Exclusive per-run workspace claims, preserved backups for malformed settings, native Windows path arguments, visible warnings when optional model metrics cannot be collected, and compatibility warnings for COLMAP releases with known pipeline defects.

## Try the existing result first
Choose Open existing point cloud and select:
`%USERPROFILE%\Documents\COLMAP Tests\rust-launcher-test-01\sparse\0\points3D.bin`

The launch copy was visually verified rendering all 17,461 retained points from this model. This verifies loading/rendering, not fresh GUI reconstruction.

## Run photos
The defaults point to the installed COLMAP and downloaded South Building sample. Workspace defaults to a new timestamped folder. Click Start reconstruction; the preview should load the first resulting sparse model after successful completion. Additional models can be opened separately.
After any run, choose New workspace name before starting again. Never use the photo folder as workspace.

## Verification status — important
- 61 routine automated tests passed: input validation, exclusive/resumable workspace claims, strict project identity, stage fingerprint reuse, pipeline command construction, native Windows paths, genuine progress parsing, stage state, cooperative/fallback cancellation, defensive sparse/dense and modern rig parsing, safe source-image resolution, deterministic sampling, metrics, recoverable settings, diagnostics/version warnings, project naming, projection math, and GUI error handling.
- cargo fmt --check and cargo clippy --features gui --all-targets -- -D warnings passed.
- Windows GitHub Actions now enforces formatting, tests, Clippy, and release builds on pushes and pull requests.
- Release executable built and desktop copy checksum verified.
- The quick sparse GUI path and live COLMAP cancellation were manually verified earlier.
- The RTX pipeline completed the 128-photo South Building dataset in a fresh workspace. It produced 128 depth maps, 128 normal maps, and an 88,238,408-byte `fused.ply` containing 3,268,080 vertices. The run log confirms SIFT GPU extraction, SIFT matching bound to GPU 0, and CUDA PatchMatch with GPU index 0.
- The verified system uses an NVIDIA GeForce RTX 5050 with driver 617.14 and CUDA driver capability 13.4.
- The installed COLMAP 4.2.0 CUDA build performs the GPU stages above, but its Ceres library lacks CUDA/cuDSS and Caspar is disabled, so mapping bundle adjustment remains on CPU. COLMAP 4.2.1 or newer is recommended; GPU bundle adjustment requires a compatible custom build.

## Build
```text
cargo test --features gui
cargo fmt --check
cargo clippy --features gui --all-targets -- -D warnings
cargo build --release --features gui --bin colmap-studio
```

## Limitations
This remains a point-cloud application, not a mesh/texturing tool. There are no connected surfaces, photo textures, selective stage-reset UI, source-image pixel viewer, or installer. On-screen technical logs are bounded and may skip lines under heavy output; the complete append-only disk log is preserved. Projects intentionally reject changed photos, COLMAP executables, or stage settings rather than risk stale cache reuse. COLMAP is separately installed and retains its own dependency licensing obligations; see `THIRD_PARTY_NOTICES.md`.
