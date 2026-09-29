# COLMAP Rust Launcher — v0.1.0

**GUI preview now available:** see [GUI_README.md](GUI_README.md). Launch `colmap-studio.exe` for the native window. The sections below describe the original console launcher, whose behavior is unchanged.

A working Windows console prototype, not yet a graphical app. Rust orchestrates your existing COLMAP installation; COLMAP still performs reconstruction. No speed advantage over the same COLMAP commands is claimed.

## Open it
Double-click `colmap-launcher.exe` in your launcher folder (for example,
`%USERPROFILE%\Desktop\COLMAP Rust Launcher`).

1. Press Enter to accept your COLMAP installation path.
2. Press Enter to use the downloaded South Building photos, or paste a folder directly containing your own photos.
3. Press Enter to create a new timestamped workspace, or enter a new/empty folder whose parent already exists.
4. Type `yes` and press Enter to start.
5. Wait for `SUCCESS`. The window stays open until you press Enter again.

Default preset: LOW quality, sparse only, individual images, GPU device 0. No dense reconstruction, mesh, or texture generation in this version.

## Viewing results
In COLMAP use File > Import model and select the printed model folder (usually `<workspace>\sparse\0`). Do not select the whole workspace. For the already completed verification run, use:

`%USERPROFILE%\Documents\COLMAP Tests\rust-launcher-test-01\sparse\0`

## Safety and limitations
- Non-empty workspaces are refused. Use a new workspace for each run.
- Source photos and the COLMAP installation are not modified by the launcher.
- `run.log` contains actual process output; percentages are not invented.
- Failed runs retain logs and any partial results; nothing is automatically deleted.
- GPU errors fail visibly. There is no automatic CPU retry.
- Some COLMAP operations use CPU even with GPU enabled.
- Cancel before starting by answering anything other than `yes`.
- Graceful in-progress cancellation is NOT implemented. Closing the window may leave processing running; do not treat that as a supported cancel feature.
- The model gate checks non-empty output headers, not complete geometric correctness. The verification run was additionally inspected with COLMAP model_analyzer.
- Windows console Unicode input depends on terminal configuration; the argument-based interface uses native OS strings.
- No COLMAP binaries or sample photos are bundled here. Commercial distribution requires a separate dependency/license audit.

## Command line (PowerShell)
```powershell
& '.\colmap-launcher.exe' --colmap "$env:USERPROFILE\Desktop\colmap" --images "$env:USERPROFILE\Documents\COLMAP Tests\south-building\south-building\images" --workspace "$env:USERPROFILE\Documents\COLMAP Tests\my-new-run"
```

## Build and tests
Run these commands from the repository root:

```text
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The real-data integration test is opt-in because it runs COLMAP and creates output. Set COLMAP_TEST_INSTALL, COLMAP_TEST_IMAGES, and COLMAP_TEST_WORKSPACE (a fresh directory), then run:

```text
cargo test --release --test cli real_sample_reconstruction -- --ignored
```

## Verified run
- COLMAP 4.2.0 installed at the user's Desktop location.
- All 128 photos registered in one model; 17,461 reconstructed points.
- Mean reprojection error reported by model_analyzer: 0.933153 pixels.
- GPU SIFT extraction and matching confirmed in logs.
- Bundle adjustment fell back to CPU because this build's Ceres lacks CUDA/cuDSS support.
- Real integration test completed in 46.38 seconds (one run, not a performance benchmark).
- Existing non-empty workspace rejected; interactive cancellation checked.

Next milestone: folder pickers, background job controls with reliable cancellation, saved projects, and a 3D preview.
