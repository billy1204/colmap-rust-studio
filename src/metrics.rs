use std::env;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use crate::job;

#[derive(Clone, Debug, Default)]
pub struct ModelMetrics {
    pub registered_images: Option<u64>,
    pub points: Option<u64>,
    pub observations: Option<u64>,
    pub mean_track_length: Option<f64>,
    pub mean_reprojection_error_px: Option<f64>,
}

pub enum AnalysisOutcome {
    Completed(ModelMetrics),
    Cancelled,
}

pub fn parse(text: &str) -> ModelMetrics {
    let mut metrics = ModelMetrics::default();
    for line in text.lines() {
        let clean = line
            .rsplit_once("] ")
            .map(|(_, value)| value)
            .unwrap_or(line)
            .trim();
        let Some((label, raw)) = clean.split_once(':') else {
            continue;
        };
        let value = raw.trim().trim_end_matches("px").trim();
        match label.trim() {
            "Registered images" => metrics.registered_images = value.parse().ok(),
            "Points" => metrics.points = value.parse().ok(),
            "Observations" => metrics.observations = value.parse().ok(),
            "Mean track length" => metrics.mean_track_length = value.parse().ok(),
            "Mean reprojection error" => metrics.mean_reprojection_error_px = value.parse().ok(),
            _ => {}
        }
    }
    metrics
}

pub fn analyze(
    colmap: &Path,
    model: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<AnalysisOutcome, String> {
    let mut command = Command::new(colmap);
    command.args(["model_analyzer", "--path"]);
    command.arg(model);
    if let Some(bin) = colmap.parent() {
        let mut paths = vec![bin.to_path_buf()];
        if let Some(current) = env::var_os("PATH") {
            paths.extend(env::split_paths(&current));
        }
        if let Ok(path) = env::join_paths(paths) {
            command.env("PATH", path);
        }
        if let Some(root) = bin.parent() {
            command.env("QT_PLUGIN_PATH", root.join("plugins"));
        }
    }
    let output = match job::capture(&mut command, cancel, Duration::from_secs(30))? {
        job::CaptureOutcome::Completed(output) => output,
        job::CaptureOutcome::Cancelled => return Ok(AnalysisOutcome::Cancelled),
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(AnalysisOutcome::Completed(parse(&text)))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_prefixed_colmap_model_analyzer_output() {
        let text = "I20260929 timer.cc:90] Registered images: 128
Points: 17461
Observations: 75416
Mean track length: 4.319
Mean observations per image: 589
Mean reprojection error: 0.933153px
";
        let m = parse(text);
        assert_eq!(m.registered_images, Some(128));
        assert_eq!(m.points, Some(17461));
        assert_eq!(m.observations, Some(75416));
        assert_eq!(m.mean_track_length, Some(4.319));
        assert_eq!(m.mean_reprojection_error_px, Some(0.933153));
    }
    #[test]
    fn malformed_or_missing_values_are_unavailable() {
        let m = parse(
            "Points: nope
Other: 8
",
        );
        assert_eq!(m.points, None);
        assert_eq!(m.registered_images, None);
    }
}
