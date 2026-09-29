#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
}

#[derive(Clone, Debug)]
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
            }
        }
        stages[index].label = next_label;
        stages[index].status = status;
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
