#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PipelineKind {
    #[default]
    QuickSparse,
    RtxDense,
}

impl PipelineKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::QuickSparse => "Quick Sparse Preview",
            Self::RtxDense => "RTX Dense Point Cloud",
        }
    }
    pub fn summary(self) -> &'static str {
        match self {
            Self::QuickSparse => "Fastest result • sparse colored points • GPU feature work",
            Self::RtxDense => {
                "Detailed points • CUDA SIFT + CUDA PatchMatch • CPU mapping and fusion"
            }
        }
    }
    pub fn output(self) -> &'static str {
        match self {
            Self::QuickSparse => "Output: sparse/<model>/points3D.bin",
            Self::RtxDense => "Output: dense/<model>/fused.ply",
        }
    }
    pub fn stages(self) -> &'static [&'static str] {
        match self {
            Self::QuickSparse => &["Sparse reconstruction — COLMAP automatic reconstructor"],
            Self::RtxDense => &[
                "CUDA SIFT feature extraction",
                "CUDA exhaustive feature matching",
                "CPU sparse mapping and bundle adjustment",
                "Image undistortion",
                "CUDA PatchMatch stereo",
                "CPU dense point fusion",
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Cancelled,
    Skipped,
}

#[derive(Clone, Debug)]
pub struct StageView {
    base_label: String,
    pub label: String,
    pub status: StageStatus,
    /// Progress read from COLMAP's log while the stage runs.
    pub progress: Option<StageProgress>,
}

/// How far a running stage has got, from COLMAP's own log lines.
#[derive(Clone, Debug, PartialEq)]
pub struct StageProgress {
    /// What COLMAP is doing, e.g. "Extracting features 12/41".
    pub detail: String,
    /// 0..=1 when COLMAP reports a total; None when only a count is known.
    pub fraction: Option<f32>,
    /// When the current step started, for a time-left estimate.
    pub since: std::time::Instant,
}

impl StageProgress {
    /// Rough time left, once enough of the step is done to judge.
    pub fn remaining(&self) -> Option<std::time::Duration> {
        let fraction = self.fraction.filter(|f| *f >= 0.05 && *f < 1.0)?;
        let elapsed = self.since.elapsed();
        (elapsed.as_secs_f32() >= 3.0).then(|| elapsed.mul_f32((1.0 - fraction) / fraction))
    }
}

/// A progress line from COLMAP's log.
#[derive(Clone, Debug, PartialEq)]
pub enum LogProgress {
    /// `done` of `total` items, e.g. images processed.
    Count {
        what: &'static str,
        done: u64,
        total: u64,
    },
    /// The mapper has registered this many images so far.
    Registered(u64),
    /// The automatic reconstructor started a new phase.
    Phase(&'static str),
}

/// Recognise COLMAP's progress lines (glog prefix and all), e.g.
/// `feature_extraction.cc:243] Processed file [7/41]`.
pub fn parse_progress(line: &str) -> Option<LogProgress> {
    // Drop a glog prefix ("I20261106 12:24:01.80 539672 file.cc:243] ").
    let glog = line.len() > 1
        && line.starts_with(['I', 'W', 'E', 'F'])
        && line.as_bytes()[1].is_ascii_digit();
    let text = if glog {
        line.split_once("] ").map_or(line, |(_, rest)| rest)
    } else {
        line
    }
    .trim();
    // "[a/b]" right after `prefix`.
    let bracket = |prefix: &str| -> Option<(u64, u64)> {
        let rest = text.strip_prefix(prefix)?.strip_prefix('[')?;
        let (done, rest) = rest.split_once('/')?;
        let total = rest.split(|c: char| !c.is_ascii_digit()).next()?;
        Some((done.trim().parse().ok()?, total.parse().ok()?))
    };
    let count = |what, (done, total): (u64, u64)| {
        (total > 0).then_some(LogProgress::Count {
            what,
            done: done.min(total),
            total,
        })
    };
    if let Some(n) = bracket("Processed file ") {
        return count("Extracting features", n);
    }
    // Reported as each item starts, so the previous one is done.
    if let Some((i, n)) = bracket("Undistorting image ") {
        return count("Undistorting images", (i.saturating_sub(1), n));
    }
    if let Some((i, n)) = bracket("Fusing image ") {
        return count("Fusing depth maps", (i.saturating_sub(1), n));
    }
    // "Processing block [a/b, c/d]": block (a, c) of a b × d grid.
    if let Some(rest) = text.strip_prefix("Processing block [") {
        let mut nums = rest
            .split(|c: char| !c.is_ascii_digit())
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<u64>().ok());
        let (a, b, c, d) = (nums.next()?, nums.next()?, nums.next()?, nums.next()?);
        let done = a.saturating_sub(1) * d + c.saturating_sub(1);
        return count("Matching features", (done, b * d));
    }
    // "=== Processing view 18 / 41 for IMG.JPG ==="
    if let Some(rest) = text.strip_prefix("=== Processing view ") {
        let (i, rest) = rest.split_once('/')?;
        let n = rest.split_whitespace().next()?;
        return count(
            "PatchMatch depth maps",
            (
                i.trim().parse::<u64>().ok()?.saturating_sub(1),
                n.parse().ok()?,
            ),
        );
    }
    // "Registering image #35 (num_reg_frames=40)"; older COLMAP: "(40)".
    if let Some(rest) = text.strip_prefix("Registering image #") {
        let inside = rest.split_once('(')?.1.split_once(')')?.0;
        let n = inside.rsplit('=').next()?.trim();
        return Some(LogProgress::Registered(n.parse().ok()?));
    }
    const PHASES: [&str; 3] = [
        "Feature extraction",
        "Feature matching",
        "Sparse reconstruction",
    ];
    let phase = text.strip_prefix("=== ")?.strip_suffix(" ===")?;
    PHASES
        .into_iter()
        .find(|p| *p == phase)
        .map(LogProgress::Phase)
}

/// Apply a progress line to the running stage. `images` is the number of
/// photos in the run, if known, to turn a registered count into a fraction.
pub fn apply_progress(stages: &mut [StageView], progress: LogProgress, images: Option<u64>) {
    let Some(stage) = stages.iter_mut().find(|s| s.status == StageStatus::Running) else {
        return;
    };
    let now = std::time::Instant::now();
    // Keep the start time while the same step continues, for the estimate.
    let since = |detail_start: &str, stage: &StageView| {
        stage
            .progress
            .as_ref()
            .filter(|p| p.detail.starts_with(detail_start))
            .map_or(now, |p| p.since)
    };
    let next = match progress {
        // A single item (e.g. one matching block) says nothing about how far
        // along it is, so show the step without a count.
        LogProgress::Count { what, total: 1, .. } => StageProgress {
            detail: what.to_owned(),
            fraction: None,
            since: since(what, stage),
        },
        LogProgress::Count { what, done, total } => StageProgress {
            detail: format!("{what} {done}/{total}"),
            fraction: Some(done as f32 / total as f32),
            since: since(what, stage),
        },
        LogProgress::Registered(n) => StageProgress {
            detail: match images {
                Some(total) => format!("Registering images {n}/{total}"),
                None => format!("Registering images: {n} so far"),
            },
            fraction: images
                .filter(|t| *t > 0)
                .map(|t| (n as f32 / t as f32).min(1.0)),
            since: since("Registering images", stage),
        },
        LogProgress::Phase(phase) => StageProgress {
            detail: phase.to_owned(),
            fraction: None,
            since: now,
        },
    };
    stage.progress = Some(next);
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum PipelineEvent {
    Started(String),
    Finished(String),
    Failed(String),
    Cancelled(String),
}

pub fn initial_stages(pipeline: PipelineKind) -> Vec<StageView> {
    pipeline
        .stages()
        .iter()
        .map(|label| StageView {
            base_label: (*label).to_owned(),
            label: (*label).to_owned(),
            status: StageStatus::Pending,
            progress: None,
        })
        .collect()
}

pub fn apply_stage_event(stages: &mut [StageView], event: PipelineEvent) {
    let (label, status, terminal) = match event {
        PipelineEvent::Started(label) => (label, StageStatus::Running, false),
        PipelineEvent::Finished(label) => (label, StageStatus::Complete, false),
        PipelineEvent::Failed(label) => (label, StageStatus::Failed, true),
        PipelineEvent::Cancelled(label) => (label, StageStatus::Cancelled, true),
    };
    let (model, base_label) = label
        .split_once(": ")
        .map_or((None, label.as_str()), |(model, suffix)| {
            (Some(model), suffix)
        });
    if let Some(index) = stages
        .iter()
        .position(|stage| stage.base_label == base_label)
    {
        let next_label = model
            .map(|model| format!("{} — {model}", stages[index].base_label))
            .unwrap_or_else(|| stages[index].base_label.clone());
        if matches!(status, StageStatus::Running)
            && model.is_some()
            && stages[index].label != next_label
        {
            for stage in &mut stages[index..] {
                stage.status = StageStatus::Pending;
                stage.label = stage.base_label.clone();
                stage.progress = None;
            }
        }
        stages[index].label = next_label;
        stages[index].status = status;
        stages[index].progress = None;
        if terminal {
            for stage in &mut stages[index + 1..] {
                if stage.status == StageStatus::Pending {
                    stage.status = StageStatus::Skipped;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pipeline_cards_describe_truthful_outputs_and_stages() {
        let quick = PipelineKind::QuickSparse;
        assert_eq!(quick.title(), "Quick Sparse Preview");
        assert!(quick.summary().contains("Fastest"));
        assert!(quick.output().contains("points3D.bin"));
        assert_eq!(
            quick.stages(),
            &["Sparse reconstruction — COLMAP automatic reconstructor"]
        );
        let dense = PipelineKind::RtxDense;
        assert_eq!(dense.title(), "RTX Dense Point Cloud");
        assert!(dense.summary().contains("CUDA"));
        assert!(dense.output().contains("fused.ply"));
        assert_eq!(dense.stages().len(), 6);
        assert!(dense.stages().iter().any(|s| s.contains("CPU")));
        assert!(dense.stages().iter().any(|s| s.contains("CUDA PatchMatch")));
    }

    #[test]
    fn stage_events_are_truthful_and_skip_later_work_after_failure() {
        let mut stages = initial_stages(PipelineKind::RtxDense);
        let first = stages[0].label.clone();
        let second = stages[1].label.clone();
        apply_stage_event(&mut stages, PipelineEvent::Started(first.clone()));
        assert_eq!(stages[0].status, StageStatus::Running);
        apply_stage_event(&mut stages, PipelineEvent::Finished(first));
        assert_eq!(stages[0].status, StageStatus::Complete);
        apply_stage_event(&mut stages, PipelineEvent::Started(second.clone()));
        apply_stage_event(&mut stages, PipelineEvent::Failed(second));
        assert_eq!(stages[1].status, StageStatus::Failed);
        assert!(
            stages[2..]
                .iter()
                .all(|stage| stage.status == StageStatus::Skipped)
        );
    }

    #[test]
    fn parses_colmap_progress_lines() {
        let p = |l| parse_progress(l);
        let count = |what, done, total| Some(LogProgress::Count { what, done, total });
        assert_eq!(
            p("I20261106 12:24:01.803929 539672 feature_extraction.cc:243] Processed file [5/41]"),
            count("Extracting features", 5, 41)
        );
        assert_eq!(
            p("I20261106 12:24:06.330338 539752 pairing.cc:186] Processing block [2/3, 1/3]"),
            count("Matching features", 3, 9)
        );
        assert_eq!(
            p("I1106 incremental_pipeline.cc:618] Registering image #35 (num_reg_frames=40)"),
            Some(LogProgress::Registered(40))
        );
        assert_eq!(
            p("Registering image #12 (7)"),
            Some(LogProgress::Registered(7))
        );
        assert_eq!(
            p("I1106 undistorters.cc:181] Undistorting image [12/41]"),
            count("Undistorting images", 11, 41)
        );
        assert_eq!(
            p("I1106 patch_match.cc:389] === Processing view 18 / 41 for IMG_1041.JPG ==="),
            count("PatchMatch depth maps", 17, 41)
        );
        assert_eq!(
            p("I1106 fusion.cc:257] Fusing image [30/41] with index 12"),
            count("Fusing depth maps", 29, 41)
        );
        assert_eq!(
            p("I1106 automatic_reconstruction.cc:321] === Feature matching ==="),
            Some(LogProgress::Phase("Feature matching"))
        );
        for other in [
            "Elapsed time: 0.092 [minutes]",
            "=== Feature matching & geometric verification ===",
            "Processed file [x/41]",
            "Processed file [0/0]",
            "",
        ] {
            assert_eq!(p(other), None, "{other}");
        }
    }

    #[test]
    fn progress_goes_to_the_running_stage_and_clears_when_it_ends() {
        let mut stages = initial_stages(PipelineKind::QuickSparse);
        let label = stages[0].label.clone();
        // Nothing running yet: ignored.
        apply_progress(&mut stages, LogProgress::Registered(3), Some(10));
        assert!(stages[0].progress.is_none());
        apply_stage_event(&mut stages, PipelineEvent::Started(label.clone()));
        apply_progress(
            &mut stages,
            LogProgress::Phase("Sparse reconstruction"),
            Some(10),
        );
        assert_eq!(stages[0].progress.as_ref().unwrap().fraction, None);
        apply_progress(&mut stages, LogProgress::Registered(4), Some(10));
        let p = stages[0].progress.clone().unwrap();
        assert_eq!(
            (p.detail.as_str(), p.fraction),
            ("Registering images 4/10", Some(0.4))
        );
        apply_progress(&mut stages, LogProgress::Registered(12), Some(10));
        assert_eq!(stages[0].progress.as_ref().unwrap().fraction, Some(1.0));
        apply_progress(&mut stages, LogProgress::Registered(5), None);
        assert_eq!(stages[0].progress.as_ref().unwrap().fraction, None);
        // One matching block: no meaningful count.
        apply_progress(
            &mut stages,
            LogProgress::Count {
                what: "Matching features",
                done: 0,
                total: 1,
            },
            Some(10),
        );
        let p = stages[0].progress.clone().unwrap();
        assert_eq!((p.detail.as_str(), p.fraction), ("Matching features", None));
        apply_stage_event(&mut stages, PipelineEvent::Finished(label));
        assert!(stages[0].progress.is_none());
    }

    #[test]
    fn dynamic_model_stage_updates_base_stage_by_suffix() {
        let mut stages = initial_stages(PipelineKind::RtxDense);
        apply_stage_event(
            &mut stages,
            PipelineEvent::Started("Model 0: CUDA PatchMatch stereo".into()),
        );
        assert_eq!(stages[4].status, StageStatus::Running);
        assert!(stages[4].label.contains("Model 0"));
        apply_stage_event(
            &mut stages,
            PipelineEvent::Finished("Model 0: CUDA PatchMatch stereo".into()),
        );
        assert_eq!(stages[4].status, StageStatus::Complete);
    }

    #[test]
    fn later_model_failure_does_not_leave_prior_fusion_marked_complete() {
        let mut stages = initial_stages(PipelineKind::RtxDense);
        for label in [
            "Model 0: Image undistortion",
            "Model 0: CUDA PatchMatch stereo",
            "Model 0: CPU dense point fusion",
        ] {
            apply_stage_event(&mut stages, PipelineEvent::Started(label.into()));
            apply_stage_event(&mut stages, PipelineEvent::Finished(label.into()));
        }
        assert_eq!(stages[5].status, StageStatus::Complete);

        apply_stage_event(
            &mut stages,
            PipelineEvent::Started("Model 1: Image undistortion".into()),
        );
        assert_eq!(stages[5].status, StageStatus::Pending);
        apply_stage_event(
            &mut stages,
            PipelineEvent::Finished("Model 1: Image undistortion".into()),
        );
        apply_stage_event(
            &mut stages,
            PipelineEvent::Started("Model 1: CUDA PatchMatch stereo".into()),
        );
        apply_stage_event(
            &mut stages,
            PipelineEvent::Failed("Model 1: CUDA PatchMatch stereo".into()),
        );
        assert_eq!(stages[4].status, StageStatus::Failed);
        assert_eq!(stages[5].status, StageStatus::Skipped);
    }
}
