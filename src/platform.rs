//! Platform-specific defaults: home folder, COLMAP executable location,
//! helper program names and opening files in the desktop shell.

use std::path::{Path, PathBuf};

/// File name of the COLMAP executable on this platform.
pub const COLMAP_EXE: &str = if cfg!(windows) {
    "colmap.exe"
} else {
    "colmap"
};

/// File name of the NVIDIA System Management Interface tool.
pub const NVIDIA_SMI: &str = if cfg!(windows) {
    "nvidia-smi.exe"
} else {
    "nvidia-smi"
};

/// The user's home folder (`%USERPROFILE%` on Windows, `$HOME` elsewhere).
pub fn home_dir() -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Folder that holds sample data and new studio projects by default.
pub fn default_samples_root() -> PathBuf {
    home_dir().join("Documents").join("COLMAP Tests")
}

/// Search `PATH` for an executable file.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Default COLMAP *installation folder* (the folder that contains `bin/`).
///
/// * Windows: `%USERPROFILE%\Desktop\colmap` (the portable COLMAP zip).
/// * Linux/Unix: derived from `$COLMAP_BIN`, else the `colmap` found on
///   `PATH` (e.g. `/usr/bin/colmap` -> `/usr`), else well-known install
///   folders such as `~/.local`, `/usr/local` and `/opt/colmap` (an autostart
///   session often has a minimal `PATH`), else `/usr/local`.
pub fn default_install() -> PathBuf {
    if cfg!(windows) {
        return home_dir().join("Desktop").join("colmap");
    }
    let exe = std::env::var_os("COLMAP_BIN")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| find_on_path(COLMAP_EXE));
    exe.and_then(|exe| install_root_of(&exe))
        .or_else(|| find_installs().into_iter().next())
        .unwrap_or_else(|| PathBuf::from("/usr/local"))
}

/// Search common places for COLMAP installs and return each install root
/// (the folder containing `bin/colmap`), best match first, without duplicates.
pub fn find_installs() -> Vec<PathBuf> {
    let mut exes: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("COLMAP_BIN").map(PathBuf::from) {
        exes.push(p);
    }
    if let Some(p) = find_on_path(COLMAP_EXE) {
        exes.push(p);
    }
    let home = home_dir();
    let roots: Vec<PathBuf> = if cfg!(windows) {
        vec![home.join("Desktop").join("colmap"), PathBuf::from(r"C:\Program Files\COLMAP")]
    } else {
        vec![
            home.join(".local"),
            PathBuf::from("/usr/local"),
            PathBuf::from("/usr"),
            PathBuf::from("/opt/colmap"),
            home.join("colmap-install"),
            home.join("colmap"),
        ]
    };
    exes.extend(roots.iter().map(|r| r.join("bin").join(COLMAP_EXE)));
    let mut found: Vec<PathBuf> = Vec::new();
    for exe in exes.into_iter().filter(|e| e.is_file()) {
        let root = install_root_of(&exe).unwrap_or(exe);
        if !found.contains(&root) {
            found.push(root);
        }
    }
    found
}

fn install_root_of(exe: &Path) -> Option<PathBuf> {
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let bin = exe.parent()?;
    // Only an exact `<root>/bin/colmap` layout maps back to an install root.
    if bin.file_name()? == "bin" && exe.file_name()? == COLMAP_EXE {
        bin.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Resolve the COLMAP executable for a user-facing "installation" setting.
///
/// Accepts either an installation folder (`<install>/bin/colmap[.exe]`) or,
/// for convenience on Linux, a direct path to the executable itself.
pub fn colmap_executable(install: &Path) -> PathBuf {
    if install.is_file() {
        return install.to_path_buf();
    }
    install.join("bin").join(COLMAP_EXE)
}

/// `QT_PLUGIN_PATH` only matters for the self-contained Windows COLMAP build.
/// System packages on Linux find their Qt plugins on their own, so pointing
/// Qt at a non-existent folder is avoided there.
///
/// When Studio runs from an AppImage, the AppImage's own environment
/// (`APPDIR`, `APPIMAGE`, and `PATH`/`LD_LIBRARY_PATH` entries inside the
/// mount) is removed so COLMAP loads the system's libraries, not ours.
pub fn configure_colmap_env(command: &mut std::process::Command, colmap: &Path) {
    let Some(bin) = colmap.parent() else {
        return;
    };
    let appdir = std::env::var_os("APPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let outside_appdir = |p: &PathBuf| appdir.as_ref().is_none_or(|a| !p.starts_with(a));
    let mut paths = vec![bin.to_path_buf()];
    if let Some(current) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&current).filter(outside_appdir));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
    if appdir.is_some() {
        for key in ["APPDIR", "APPIMAGE", "ARGV0", "OWD"] {
            command.env_remove(key);
        }
        let libs: Vec<PathBuf> = std::env::var_os("LD_LIBRARY_PATH")
            .map(|v| std::env::split_paths(&v).filter(outside_appdir).collect())
            .unwrap_or_default();
        match std::env::join_paths(libs) {
            Ok(v) if !v.is_empty() => {
                command.env("LD_LIBRARY_PATH", v);
            }
            _ => {
                command.env_remove("LD_LIBRARY_PATH");
            }
        }
    }
    if let Some(root) = bin.parent() {
        let plugins = root.join("plugins");
        if cfg!(windows) || plugins.is_dir() {
            command.env("QT_PLUGIN_PATH", plugins);
        }
    }
}

/// Whether the install looks complete enough to run COLMAP. On Windows the
/// portable build ships a `plugins` folder next to `bin`; Linux packages do not.
pub fn plugins_present(install: &Path) -> bool {
    if cfg!(windows) {
        install.join("plugins").is_dir()
    } else {
        true
    }
}

/// Open a folder or file with the desktop's default handler.
pub fn open_in_shell(path: &Path) -> std::io::Result<()> {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(path);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    spawn_detached(&mut command)
}

/// Show a file in the file manager, selecting it where the platform allows.
pub fn reveal_in_shell(path: &Path) -> std::io::Result<()> {
    if cfg!(windows) {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(format!("/select,{}", path.display()));
        spawn_detached(&mut c)
    } else {
        // xdg-open has no "select"; open the containing folder instead.
        open_in_shell(path.parent().unwrap_or(path))
    }
}

/// Open a text file (log) in the default viewer/editor.
pub fn open_text_file(path: &Path) -> std::io::Result<()> {
    if cfg!(windows) {
        let mut c = std::process::Command::new("notepad.exe");
        c.arg(path);
        spawn_detached(&mut c)
    } else {
        open_in_shell(path)
    }
}

fn spawn_detached(command: &mut std::process::Command) -> std::io::Result<()> {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the short-lived launcher in the background to avoid zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn executable_resolution_accepts_install_folder() {
        let p = colmap_executable(Path::new("/definitely/missing/install"));
        assert!(p.ends_with(Path::new("bin").join(COLMAP_EXE)));
    }
    #[test]
    fn install_root_maps_bin_layout() {
        let root = std::env::temp_dir().join(format!("colmap-platform-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(COLMAP_EXE);
        std::fs::write(&exe, b"").unwrap();
        assert_eq!(install_root_of(&exe).unwrap(), root.canonicalize().unwrap());
        assert_eq!(colmap_executable(&exe), exe);
        std::fs::remove_dir_all(root).unwrap();
    }
}
