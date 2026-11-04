# COLMAP Rust Studio (Linux)

A native Linux GUI (`colmap-studio`) and console launcher (`colmap-launcher`) that drive your existing [COLMAP](https://colmap.github.io/) installation. Rust handles the workflow, job control and previews; COLMAP still does the reconstruction, so no speed advantage over running the same COLMAP commands is claimed.

Tested on Pop!_OS 24.04 (COSMIC, Wayland) with an NVIDIA RTX 5050 and driver 595. See [GUI_README.md](GUI_README.md) for the GUI walkthrough.

## Requirements
- Rust 1.85+ (edition 2024), with `rustfmt` and `clippy` for the checks.
- No GTK or other `-dev` packages. Windows use winit (native Wayland, X11 fallback) with the glow (OpenGL/EGL) renderer. File pickers use the XDG desktop portal (`xdg-desktop-portal` plus your desktop's backend, which COSMIC, GNOME and KDE already ship).
- COLMAP, installed separately (see below).

## Build and run
```bash
cargo build --release --features gui
./target/release/colmap-studio           # GUI
./target/release/colmap-launcher --help   # console launcher
```

Checks:
```bash
cargo test --features gui
cargo fmt --check
cargo clippy --features gui --all-targets -- -D warnings
```

The real-data integration test is opt-in because it runs COLMAP and writes output. Set `COLMAP_TEST_INSTALL`, `COLMAP_TEST_IMAGES` and `COLMAP_TEST_WORKSPACE` (a fresh directory), then:
```bash
cargo test --release --test cli real_sample_reconstruction -- --ignored
```

## Installing COLMAP
The studio looks for COLMAP in this order: the saved setting, `$COLMAP_BIN` (full path to the executable), then `colmap` on `PATH`. The "COLMAP installation" field accepts an install prefix containing `bin/colmap` (such as `/usr`, `/usr/local` or `~/.local`) or the executable itself.

- **Quick option:** `sudo apt install colmap` gives COLMAP 3.9.1 *without CUDA* on Ubuntu/Pop!_OS 24.04. Quick Sparse Preview works on the CPU, but the RTX Dense pipeline (CUDA PatchMatch) won't. The compatibility check reports "built without CUDA".
- **GPU option:** build COLMAP from source with CUDA. RTX 50-series cards (Blackwell, `sm_120`) need CUDA 12.8 or newer from [NVIDIA's CUDA repo](https://developer.nvidia.com/cuda-downloads); Ubuntu's `nvidia-cuda-toolkit` 12.0 is too old. Use a recent COLMAP, which includes the Blackwell PatchMatch fix ([colmap#3514](https://github.com/colmap/colmap/pull/3514)).

### Building COLMAP with CUDA (verified on Pop!_OS 24.04, CUDA 13.0, RTX 5050)
```bash
# CUDA from NVIDIA's repo
wget https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb
sudo dpkg -i cuda-keyring_1.1-1_all.deb && sudo apt-get update
sudo apt-get install -y cuda-toolkit-13-0

# Build dependencies
sudo apt-get install -y git cmake ninja-build build-essential \
  libboost-program-options-dev libboost-graph-dev libboost-system-dev \
  libeigen3-dev libfreeimage-dev libmetis-dev libgoogle-glog-dev \
  libgtest-dev libgmock-dev libsqlite3-dev libglew-dev \
  qt6-base-dev libqt6opengl6-dev libqt6openglwidgets6 qt6-svg-dev libxkbcommon-dev \
  libcgal-dev libceres-dev libsuitesparse-dev libcurl4-openssl-dev libssl-dev \
  libopenimageio-dev openimageio-tools libopenexr-dev libopencv-dev

# Build into ~/.local (no sudo needed)
git clone --depth 1 https://github.com/colmap/colmap.git && cd colmap
mkdir build && cd build
cmake .. -GNinja -DCMAKE_BUILD_TYPE=Release -DCUDA_ENABLED=ON \
  -DCMAKE_CUDA_ARCHITECTURES=120 -DCMAKE_CUDA_COMPILER=/usr/local/cuda/bin/nvcc \
  -DCMAKE_INSTALL_PREFIX=$HOME/.local
ninja && ninja install
colmap -h | head -1   # should say "with CUDA"
```
`libopencv-dev` is needed only because Ubuntu's OpenImageIO CMake files reference OpenCV's headers. Change `120` to your GPU's compute capability on other cards.

## Linux behaviour notes
- Each COLMAP run starts in its own process group. Cancel (or closing the window during a run) sends `SIGTERM` to the whole group, then `SIGKILL` after 3 seconds. The child also gets `SIGTERM` if the studio's worker dies unexpectedly (`PR_SET_PDEATHSIG`).
- Settings live in `$XDG_CONFIG_HOME/colmap-rust-studio/settings.json` (default `~/.config/...`). Default projects go to `~/Documents/COLMAP Tests`.
- "Open output" and "Open full log" use `xdg-open`; "Reveal PLY" opens the containing folder.
- Free disk space comes from `statvfs`, GPU info from `nvidia-smi`.
- If the window can't create an OpenGL/EGL context on Wayland with the NVIDIA driver, try XWayland: `WAYLAND_DISPLAY= ./colmap-studio`.

## Console launcher
```bash
./target/release/colmap-launcher --colmap ~/.local --images ~/Pictures/south-building/images --workspace ~/Documents/COLMAP\ Tests/my-new-run
```
Run it with no arguments for the interactive prompts: accept or enter the COLMAP path, the photo folder and a new workspace, then type `yes` to start and wait for `SUCCESS`. The default preset is LOW quality, sparse only, GPU device 0.

To view results, open COLMAP's GUI, choose File > Import model and select the model folder (usually `<workspace>/sparse/0`), not the whole workspace.

## Safety and limitations
- Non-empty workspaces are refused; use a new workspace for each run.
- Source photos and the COLMAP installation are never modified.
- `run.log` holds the real process output; percentages are not invented.
- Failed runs keep their logs and partial results; nothing is deleted automatically.
- GPU errors fail visibly; there is no automatic CPU retry. Some COLMAP steps use the CPU even with the GPU enabled.
- The model check verifies non-empty output headers, not full geometric correctness.
- No COLMAP binaries or sample photos are bundled. Commercial distribution needs a separate dependency and license audit.
