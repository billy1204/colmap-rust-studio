pub mod diagnostics;
pub mod job;
pub mod metrics;
pub mod orbit;
pub mod ply;
pub mod point_cloud;
pub mod preflight;
pub mod progress;
pub mod rtx_pipeline;
pub mod settings;
pub mod sparse_scene;
pub mod studio_support;
use std::path::Path;

const WORKSPACE_LOCK: &str = ".colmap-studio.lock";

pub struct WorkspaceClaim {
    lock_path: std::path::PathBuf,
    _lock: std::fs::File,
}

impl Drop for WorkspaceClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Atomically claim a validated new or empty workspace for one launcher run.
pub fn claim_workspace(workspace: &Path) -> Result<WorkspaceClaim, String> {
    match std::fs::create_dir(workspace) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && workspace.is_dir() => {}
        Err(error) => return Err(format!("Cannot create project folder: {error}")),
    }
    let lock_path = workspace.join(WORKSPACE_LOCK);
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                "This project folder is already claimed by another launcher run".to_owned()
            } else {
                format!("Cannot claim project folder: {error}")
            }
        })?;
    let claim = WorkspaceClaim {
        lock_path,
        _lock: lock,
    };
    for entry in std::fs::read_dir(workspace)
        .map_err(|error| format!("Cannot inspect claimed project folder: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Cannot inspect project entry: {error}"))?;
        if entry.file_name() != WORKSPACE_LOCK {
            return Err(
                "Workspace is not empty. Choose a new folder; existing work will not be overwritten."
                    .into(),
            );
        }
    }
    Ok(claim)
}

pub fn validate_inputs(colmap: &Path, images: &Path, workspace: &Path) -> Result<(), String> {
    if !colmap.is_file() {
        return Err(format!("COLMAP executable not found: {}", colmap.display()));
    }
    let images = images
        .canonicalize()
        .map_err(|e| format!("Cannot open images folder: {e}"))?;
    let entries = std::fs::read_dir(&images).map_err(|e| format!("Cannot read images: {e}"))?;
    let mut found = false;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if p.is_file()
            && p.extension().is_some_and(|ext| {
                matches!(
                    ext.to_string_lossy().to_ascii_lowercase().as_str(),
                    "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp"
                )
            })
        {
            found = true;
        }
    }
    if !found {
        return Err(
            "Select the folder directly containing your photos (JPG, PNG, TIFF or BMP).".into(),
        );
    }
    let resolved = if workspace.exists() {
        if !workspace.is_dir() {
            return Err("Workspace must be a folder.".into());
        }
        if std::fs::read_dir(workspace)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
        {
            return Err("Workspace is not empty. Choose a new folder; existing work will not be overwritten.".into());
        }
        workspace.canonicalize().map_err(|e| e.to_string())?
    } else {
        let parent = workspace
            .parent()
            .ok_or("Workspace needs a parent folder")?;
        let parent = parent
            .canonicalize()
            .map_err(|_| "Workspace parent folder must already exist")?;
        let name = workspace
            .file_name()
            .ok_or("Workspace needs a folder name")?;
        parent.join(name)
    };
    if resolved.starts_with(&images) || images.starts_with(&resolved) {
        return Err("Images and workspace must be separate, non-nested folders.".into());
    }
    Ok(())
}

pub fn reconstruction_command(
    colmap: &Path,
    images: &Path,
    workspace: &Path,
) -> std::process::Command {
    let mut command = std::process::Command::new(colmap);
    command
        .arg("automatic_reconstructor")
        .arg("--image_path")
        .arg(images)
        .arg("--workspace_path")
        .arg(workspace)
        .args([
            "--data_type",
            "individual",
            "--quality",
            "low",
            "--sparse",
            "1",
            "--dense",
            "0",
            "--use_gpu",
            "1",
            "--gpu_index",
            "0",
            "--random_seed",
            "0",
        ]);
    let bin = colmap.parent().unwrap_or(Path::new("."));
    let install = bin.parent().unwrap_or(bin);
    let mut paths = vec![bin.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
    command.env("QT_PLUGIN_PATH", install.join("plugins"));
    command
}

pub fn run_logged(command: &mut std::process::Command, log: &Path) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log)
        .map_err(|e| format!("Cannot create log: {e}"))?;
    writeln!(file, "Command: {command:?}").map_err(|e| e.to_string())?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start COLMAP: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(256);
    let tx2 = tx.clone();
    let a = std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            if tx.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    let b = std::thread::spawn(move || {
        for line in BufReader::new(stderr).split(b'\n') {
            if tx2.send(line.map_err(|e| e.to_string())).is_err() {
                break;
            }
        }
    });
    let mut failure = None;
    for message in rx {
        match message {
            Ok(line) => {
                // A disconnected console must not prevent draining the child pipes.
                let _ = writeln!(
                    std::io::stdout().lock(),
                    "{}",
                    String::from_utf8_lossy(&line)
                );
                if let Err(e) = file.write_all(&line).and_then(|_| file.write_all(b"\n")) {
                    failure = Some(format!("Log write failed: {e}"));
                }
            }
            Err(e) => failure = Some(format!("Output read failed: {e}")),
        }
    }
    if a.join().is_err() || b.join().is_err() {
        failure = Some("Output reader failed".into());
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    writeln!(file, "Child exit: {status}")
        .and_then(|_| file.flush())
        .map_err(|e| e.to_string())?;
    if let Some(error) = failure {
        return Err(error);
    }
    if !status.success() {
        return Err(format!("COLMAP failed ({status}). See {}", log.display()));
    }
    Ok(())
}

pub fn sparse_models(workspace: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    use std::io::Read;
    let mut models = Vec::new();
    for entry in
        std::fs::read_dir(workspace.join("sparse")).map_err(|e| format!("No sparse output: {e}"))?
    {
        let path = entry.map_err(|e| e.to_string())?.path();
        if !path.is_dir() {
            continue;
        }
        let mut valid = true;
        for name in ["cameras.bin", "images.bin", "points3D.bin"] {
            let mut header = [0u8; 8];
            let ok = std::fs::File::open(path.join(name))
                .and_then(|mut file| {
                    let len = file.metadata()?.len();
                    file.read_exact(&mut header)?;
                    Ok(len > 8 && u64::from_le_bytes(header) > 0)
                })
                .unwrap_or(false);
            valid &= ok;
        }
        if valid {
            models.push(path);
        }
    }
    models.sort();
    if models.is_empty() {
        return Err(
            "COLMAP finished but no non-empty sparse model was found. Inspect run.log.".into(),
        );
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_nonempty_sparse_model_files() {
        let base = fixture("models");
        assert!(sparse_models(&base).is_err());
        let model = base.join("sparse").join("0");
        std::fs::create_dir_all(&model).unwrap();
        assert!(sparse_models(&base).is_err());
        for name in ["cameras.bin", "images.bin", "points3D.bin"] {
            std::fs::write(model.join(name), 0u64.to_le_bytes()).unwrap();
        }
        assert!(sparse_models(&base).is_err());
        // Header-only fixtures test the structural gate, not full binary validity.
        for name in ["cameras.bin", "images.bin", "points3D.bin"] {
            std::fs::write(
                model.join(name),
                [1u64.to_le_bytes().as_slice(), &[0u8; 64]].concat(),
            )
            .unwrap();
        }
        assert_eq!(sparse_models(&base).unwrap(), vec![model]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn captures_both_streams_and_reports_nonzero_child_exit() {
        let base = fixture("logging");
        let log = base.join("run.log");
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.args([
            "/D",
            "/C",
            "echo stdout-marker & echo stderr-marker 1>&2 & exit /b 7",
        ]);
        assert!(run_logged(&mut cmd, &log).is_err());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("stdout-marker"));
        assert!(text.contains("stderr-marker"));
        assert!(text.contains("7"));
        let mut ok = std::process::Command::new("cmd.exe");
        ok.args(["/D", "/C", "echo success-marker"]);
        assert!(run_logged(&mut ok, &base.join("success.log")).is_ok());
        assert!(
            run_logged(&mut ok, &log).is_err(),
            "must not overwrite a log"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn builds_sparse_only_gpu_command_without_shell_interpolation() {
        let cmd = reconstruction_command(
            Path::new("C:/engine/bin/colmap.exe"),
            Path::new("C:/my photos & stuff"),
            Path::new("C:/my result"),
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|x| x.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "automatic_reconstructor",
                "--image_path",
                "C:/my photos & stuff",
                "--workspace_path",
                "C:/my result",
                "--data_type",
                "individual",
                "--quality",
                "low",
                "--sparse",
                "1",
                "--dense",
                "0",
                "--use_gpu",
                "1",
                "--gpu_index",
                "0",
                "--random_seed",
                "0"
            ]
        );
        assert!(cmd.get_envs().any(|(key, _)| key == "QT_PLUGIN_PATH"));
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "colmap-rust-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn validates_images_and_protects_existing_workspaces() {
        let base = fixture("validation");
        let exe = std::env::current_exe().unwrap();
        let images = base.join("photos with spaces");
        let workspace = base.join("result");
        assert!(validate_inputs(&exe, &images, &workspace).is_err());
        std::fs::create_dir(&images).unwrap();
        assert!(validate_inputs(&exe, &images, &workspace).is_err());
        std::fs::write(images.join("photo.jpg"), b"test fixture, not a real image").unwrap();
        assert!(validate_inputs(&exe, &images, &workspace).is_ok());
        assert!(!workspace.exists());
        std::fs::create_dir(&workspace).unwrap();
        assert!(validate_inputs(&exe, &images, &workspace).is_ok());
        std::fs::write(workspace.join("keep.txt"), b"original").unwrap();
        assert!(validate_inputs(&exe, &images, &workspace).is_err());
        assert_eq!(
            std::fs::read(workspace.join("keep.txt")).unwrap(),
            b"original"
        );
        assert!(validate_inputs(&exe, &images, &images.join("output")).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn workspace_claim_is_exclusive_and_preserves_existing_files() {
        let base = fixture("workspace-claim");
        let workspace = base.join("result");
        let claim = claim_workspace(&workspace).unwrap();
        assert!(workspace.join(WORKSPACE_LOCK).is_file());
        assert!(claim_workspace(&workspace).is_err());
        drop(claim);
        assert!(!workspace.join(WORKSPACE_LOCK).exists());

        std::fs::write(workspace.join("keep.txt"), b"keep").unwrap();
        assert!(claim_workspace(&workspace).is_err());
        assert_eq!(std::fs::read(workspace.join("keep.txt")).unwrap(), b"keep");
        assert!(!workspace.join(WORKSPACE_LOCK).exists());
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn rejects_missing_colmap_without_creating_workspace() {
        let base = std::env::temp_dir().join("colmap-rust-test-missing-input");
        let result = validate_inputs(
            &base.join("missing.exe"),
            &base.join("photos"),
            &base.join("output"),
        );
        assert!(result.is_err(), "missing executable must be rejected");
        assert!(!base.join("output").exists());
    }
}
