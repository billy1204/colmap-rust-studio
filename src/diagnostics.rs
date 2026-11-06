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
    /// `Some(true)` "with CUDA", `Some(false)` "without CUDA", `None` unknown.
    pub cuda: Option<bool>,
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
    use std::io::Read;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Drain both pipes concurrently so a chatty child never blocks on a full
    // pipe while we wait for it to exit.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("diagnostic command timed out".into());
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
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

/// COLMAP's help banner (the first line starting with "COLMAP") ends with
/// "with CUDA)" or "without CUDA)". Other help text is ignored.
pub fn detect_cuda(text: &str) -> Option<bool> {
    let banner = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("COLMAP "))?;
    if banner.contains("without CUDA") {
        Some(false)
    } else if banner.contains("with CUDA") {
        Some(true)
    } else {
        None
    }
}

/// Ask a COLMAP executable whether it was built with CUDA (`colmap -h`).
pub fn colmap_cuda(exe: &Path) -> Option<bool> {
    let mut command = Command::new(exe);
    command.arg("-h");
    crate::platform::configure_colmap_env(&mut command, exe);
    let output = output_with_timeout(&mut command, Duration::from_secs(10)).ok()?;
    detect_cuda(&format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

pub fn run(install: &Path, workspace: &Path) -> DiagnosticReport {
    let exe = crate::platform::colmap_executable(install);
    let mut colmap_command = Command::new(&exe);
    colmap_command.arg("-h");
    crate::platform::configure_colmap_env(&mut colmap_command, &exe);
    let help_text = output_with_timeout(&mut colmap_command, Duration::from_secs(5))
        .ok()
        .map(|output| {
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    let colmap_version = help_text.as_deref().and_then(detect_colmap_version);
    let cuda = help_text.as_deref().and_then(detect_cuda);
    let mut gpu_command = Command::new(crate::platform::NVIDIA_SMI);
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
        cuda,
        plugins_found: crate::platform::plugins_present(install),
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
    fn detects_cuda_banner() {
        assert_eq!(
            detect_cuda("COLMAP 3.9.1 -- Structure-from-Motion (Commit 0d9f8b8 with CUDA)"),
            Some(true)
        );
        assert_eq!(detect_cuda("COLMAP 3.9.1 (without CUDA)"), Some(false));
        assert_eq!(detect_cuda("usage"), None);
        assert_eq!(
            detect_cuda("COLMAP 4.3.0.dev0 (Commit 30a1a81 on 2026-11-04 with CUDA)\nUsage:"),
            Some(true)
        );
        // Only the banner counts, not option help mentioning CUDA.
        assert_eq!(
            detect_cuda("COLMAP 3.9 (Commit x without CUDA)\n  --use_gpu  run with CUDA"),
            Some(false)
        );
        assert_eq!(detect_cuda("Usage: run with CUDA"), None);
    }
    #[test]
    #[cfg(unix)]
    fn output_with_timeout_drains_large_output() {
        let mut c = Command::new("sh");
        c.args([
            "-c",
            "head -c 1000000 /dev/zero; head -c 1000000 /dev/zero 1>&2",
        ]);
        let out = output_with_timeout(&mut c, Duration::from_secs(10)).unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
        assert_eq!(out.stderr.len(), 1_000_000);
    }
    #[test]
    fn finds_colmap_version() {
        assert_eq!(
            detect_colmap_version("COLMAP 4.2.0 -- Structure-from-Motion"),
            Some("4.2.0".into())
        );
    }
}
