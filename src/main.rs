use colmap_launcher::{reconstruction_command, run_logged, sparse_models, validate_inputs};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const HELP: &str = "COLMAP Rust Launcher 0.1.0\n\nUsage:\n  colmap-launcher --colmap <installation folder> --images <photo folder> --workspace <new or empty folder>\n\nNo arguments: interactive setup.\nPreset: individual photos, LOW quality, sparse only, GPU 0.\nThe workspace parent must exist. Source photos are never modified.\nExisting non-empty workspaces are refused. Dense reconstruction is disabled.\nLogs: <workspace>/run.log. This is a console prototype, not a 3D viewer.\n";

fn prompt(label: &str, default: &Path) -> Result<PathBuf, String> {
    print!("{label} [{}]: ", default.display());
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    if io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Err("Input closed; nothing started.".into());
    }
    let line = line.trim().trim_matches('"');
    Ok(if line.is_empty() {
        default.to_path_buf()
    } else {
        PathBuf::from(line)
    })
}

fn run(interactive: bool) -> Result<(), String> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let (install, images, workspace) = if interactive {
        println!("{HELP}");
        let home = PathBuf::from(std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set")?);
        let sample_root = home.join("Documents").join("COLMAP Tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let install = prompt(
            "COLMAP installation folder",
            &home.join("Desktop").join("colmap"),
        )?;
        let images = prompt(
            "Folder directly containing photos",
            &sample_root
                .join("south-building")
                .join("south-building")
                .join("images"),
        )?;
        let workspace = prompt(
            "New workspace",
            &sample_root.join(format!("rust-run-{stamp}")),
        )?;
        (install, images, workspace)
    } else {
        let mut install = None;
        let mut images = None;
        let mut workspace = None;
        let mut pairs = args.iter();
        while let Some(key) = pairs.next() {
            let slot = match key.to_str() {
                Some("--colmap") => &mut install,
                Some("--images") => &mut images,
                Some("--workspace") => &mut workspace,
                _ => {
                    return Err(format!(
                        "Unknown option: {}. Use --help.",
                        key.to_string_lossy()
                    ));
                }
            };
            if slot.is_some() {
                return Err(format!("Duplicate option: {}", key.to_string_lossy()));
            }
            let value = pairs
                .next()
                .ok_or_else(|| format!("Missing value for {}", key.to_string_lossy()))?;
            *slot = Some(PathBuf::from(value));
        }
        (
            install.ok_or("Missing --colmap")?,
            images.ok_or("Missing --images")?,
            workspace.ok_or("Missing --workspace")?,
        )
    };
    let colmap =
        std::path::absolute(install.join("bin").join("colmap.exe")).map_err(|e| e.to_string())?;
    let images = std::path::absolute(images).map_err(|e| e.to_string())?;
    let workspace = std::path::absolute(workspace).map_err(|e| e.to_string())?;
    validate_inputs(&colmap, &images, &workspace)?;
    println!(
        "\nPhotos: {}\nWorkspace: {}\nPreset: LOW / SPARSE ONLY / GPU 0",
        images.display(),
        workspace.display()
    );
    if interactive {
        print!("Start reconstruction? Type yes: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut reply = String::new();
        io::stdin()
            .read_line(&mut reply)
            .map_err(|e| e.to_string())?;
        if !reply.trim().eq_ignore_ascii_case("yes") {
            return Err("Cancelled; nothing started.".into());
        }
    }
    validate_inputs(&colmap, &images, &workspace)?;
    if !workspace.exists() {
        std::fs::create_dir(&workspace).map_err(|e| e.to_string())?;
    }
    let log = workspace.join("run.log");
    let mut command = reconstruction_command(&colmap, &images, &workspace);
    command.current_dir(&workspace);
    run_logged(&mut command, &log)?;
    let models = sparse_models(&workspace)?;
    println!(
        "\nSUCCESS: {} non-empty sparse model(s) saved.",
        models.len()
    );
    for model in models {
        println!("Model: {}", model.display());
    }
    println!(
        "Log: {}\nOpen a model folder through COLMAP's File > Import model menu.",
        log.display()
    );
    Ok(())
}

fn main() {
    let interactive = std::env::args_os().len() == 1;
    let result = run(interactive);
    if let Err(error) = &result {
        eprintln!("\nERROR: {error}");
    }
    if interactive {
        print!("\nPress Enter to close...");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
    if result.is_err() {
        std::process::exit(1);
    }
}
