use crate::preflight;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct GpuInfo {
    pub name: String,
    pub vram_mib: u64,
    pub driver: String,
}

#[derive(Clone, Debug)]
pub struct DiagnosticReport {
    pub colmap_version: Option<String>,
    pub compatibility_warning: Option<String>,
    pub plugins_found: bool,
    pub gpu: Result<GpuInfo, String>,
    pub free_bytes: Result<u64, String>,
}

impl DiagnosticReport {
    pub fn quick_ready(&self) -> bool {
        self.colmap_version.is_some() && self.plugins_found
    }
}

fn output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => return child.wait_with_output().map_err(|e| e.to_string()),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("diagnostic command timed out".into());
            }
        }
    }
}

pub fn parse_gpu(text: &str) -> Result<GpuInfo, String> {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or("nvidia-smi returned no GPU")?;
    let mut fields = line.split(',').map(str::trim);
    let name = fields
        .next()
        .filter(|v| !v.is_empty())
        .ok_or("GPU name missing")?;
    let vram_mib = fields
        .next()
        .ok_or("VRAM missing")?
        .parse()
        .map_err(|_| "VRAM is invalid")?;
    let driver = fields
        .next()
        .filter(|v| !v.is_empty())
        .ok_or("Driver version missing")?;
    Ok(GpuInfo {
        name: name.into(),
        vram_mib,
        driver: driver.into(),
    })
}

pub fn detect_colmap_version(text: &str) -> Option<String> {
    let marker = "COLMAP ";
    let start = text.find(marker)? + marker.len();
    text[start..].split_whitespace().next().map(str::to_owned)
}

pub fn version_warning(version: &str) -> Option<String> {
    let mut fields = version.split('.');
    let parsed = (
        fields.next()?.parse::<u32>().ok()?,
        fields.next()?.parse::<u32>().ok()?,
        fields
            .next()?
            .split(|character: char| !character.is_ascii_digit())
            .next()?
            .parse::<u32>()
            .ok()?,
    );
    if parsed < (4, 2, 0) {
        Some(format!(
            "COLMAP {version} is older than the tested 4.2 command interface"
        ))
    } else if parsed < (4, 2, 1) {
        Some(format!(
            "COLMAP {version} has known CUDA, point-color, global-mapper, and PLY defects fixed in 4.2.1; upgrading is recommended"
        ))
    } else {
        None
    }
}

pub fn run(install: &Path, workspace: &Path) -> DiagnosticReport {
    let exe = install.join("bin").join("colmap.exe");
    let read_version = |output: std::process::Output| {
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        detect_colmap_version(&text)
    };
    let colmap_version =
        output_with_timeout(Command::new(&exe).arg("version"), Duration::from_secs(5))
            .ok()
            .and_then(read_version)
            .or_else(|| {
                output_with_timeout(Command::new(&exe).arg("-h"), Duration::from_secs(5))
                    .ok()
                    .and_then(read_version)
            });
    let compatibility_warning = colmap_version.as_deref().and_then(version_warning);
    let mut gpu_command = Command::new("nvidia-smi.exe");
    gpu_command.args([
        "--query-gpu=name,memory.total,driver_version",
        "--format=csv,noheader,nounits",
    ]);
    let gpu = output_with_timeout(&mut gpu_command, Duration::from_secs(5))
        .map_err(|e| format!("nvidia-smi unavailable: {e}"))
        .and_then(|output| {
            if output.status.success() {
                parse_gpu(&String::from_utf8_lossy(&output.stdout))
            } else {
                Err("nvidia-smi reported an error".into())
            }
        });
    DiagnosticReport {
        colmap_version,
        compatibility_warning,
        plugins_found: install.join("plugins").is_dir(),
        gpu,
        free_bytes: preflight::free_space(workspace),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_nvidia_csv_without_shell() {
        let g = parse_gpu("NVIDIA GeForce RTX 5050, 8192, 591.74\r\n").unwrap();
        assert_eq!(g.name, "NVIDIA GeForce RTX 5050");
        assert_eq!(g.vram_mib, 8192);
        assert_eq!(g.driver, "591.74");
    }
    #[test]
    fn finds_colmap_version() {
        assert_eq!(
            detect_colmap_version("COLMAP 4.2.0 -- Structure-from-Motion"),
            Some("4.2.0".into())
        );
    }
    #[test]
    fn warns_for_known_problematic_or_unsupported_versions() {
        assert!(version_warning("4.1.1").unwrap().contains("tested"));
        assert!(version_warning("4.2.0").unwrap().contains("4.2.1"));
        assert_eq!(version_warning("4.2.1"), None);
        assert_eq!(version_warning("4.3.0.dev0"), None);
        assert_eq!(version_warning("unknown"), None);
    }
}
