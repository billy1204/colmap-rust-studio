use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Sender, SyncSender},
};

use crate::{
    job,
    project::{PipelineId, Project, StageState},
    sparse_models,
    studio_support::PipelineEvent,
};

fn validate_fused_cloud(path: &Path) -> Result<u64, String> {
    crate::ply::load(path)
        .map(|cloud| cloud.total_points)
        .map_err(|error| format!("Invalid dense output {}: {error}", path.display()))
}

pub struct Stage {
    pub id: String,
    pub name: String,
    pub log_name: String,
    pub command: Command,
    output: StageOutput,
}

enum StageOutput {
    Database(PathBuf),
    SparseModel(PathBuf),
    Undistorted(PathBuf),
    PatchMatch(PathBuf),
    Fused(PathBuf),
}

impl StageOutput {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Database(path) => nonempty_file(path, "COLMAP database"),
            Self::SparseModel(workspace) => sparse_models(workspace).map(|_| ()),
            Self::Undistorted(path) => {
                nonempty_directory(&path.join("images"), "undistorted images")?;
                nonempty_directory(&path.join("sparse"), "undistorted sparse model")?;
                nonempty_file(
                    &path.join("stereo").join("patch-match.cfg"),
                    "PatchMatch configuration",
                )
            }
            Self::PatchMatch(path) => {
                nonempty_directory(
                    &path.join("stereo").join("depth_maps"),
                    "PatchMatch depth maps",
                )?;
                nonempty_directory(
                    &path.join("stereo").join("normal_maps"),
                    "PatchMatch normal maps",
                )
            }
            Self::Fused(path) => validate_fused_cloud(path).map(|_| ()),
        }
    }
}

fn nonempty_file(path: &Path, description: &str) -> Result<(), String> {
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    {
        Ok(())
    } else {
        Err(format!(
            "Missing or empty {description}: {}",
            path.display()
        ))
    }
}

fn nonempty_directory(path: &Path, description: &str) -> Result<(), String> {
    let has_file = std::fs::read_dir(path).ok().is_some_and(|mut entries| {
        entries.any(|entry| entry.is_ok_and(|entry| entry.path().is_file()))
    });
    if has_file {
        Ok(())
    } else {
        Err(format!(
            "Missing or empty {description}: {}",
            path.display()
        ))
    }
}

pub struct PipelineResult {
    pub models: Vec<PathBuf>,
    pub fused_clouds: Vec<PathBuf>,
}

pub enum PipelineOutcome {
    Completed(PipelineResult),
    Cancelled,
}

fn configured_command(colmap: &Path) -> Command {
    let mut command = Command::new(colmap);
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
    command
}

fn stage(
    colmap: &Path,
    id: impl Into<String>,
    name: &str,
    log_name: &str,
    args: Vec<OsString>,
    output: StageOutput,
) -> Stage {
    let mut command = configured_command(colmap);
    command.args(args);
    Stage {
        id: id.into(),
        name: name.to_owned(),
        log_name: log_name.to_owned(),
        command,
        output,
    }
}

pub fn sparse_stages(colmap: &Path, images: &Path, workspace: &Path) -> Vec<Stage> {
    let database = workspace.join("database.db").into_os_string();
    let images = images.as_os_str().to_os_string();
    let sparse = workspace.join("sparse").into_os_string();
    vec![
        stage(
            colmap,
            "feature-extraction",
            "CUDA SIFT feature extraction",
            "01-feature-extractor.log",
            vec![
                "feature_extractor".into(),
                "--database_path".into(),
                database.clone(),
                "--image_path".into(),
                images.clone(),
                "--FeatureExtraction.type".into(),
                "SIFT".into(),
                "--FeatureExtraction.use_gpu".into(),
                "1".into(),
                "--FeatureExtraction.gpu_index".into(),
                "0".into(),
                "--FeatureExtraction.max_image_size".into(),
                "1600".into(),
                "--SiftExtraction.max_num_features".into(),
                "4096".into(),
            ],
            StageOutput::Database(workspace.join("database.db")),
        ),
        stage(
            colmap,
            "feature-matching",
            "CUDA exhaustive feature matching",
            "02-exhaustive-matcher.log",
            vec![
                "exhaustive_matcher".into(),
                "--database_path".into(),
                database.clone(),
                "--FeatureMatching.type".into(),
                "SIFT_BRUTEFORCE".into(),
                "--FeatureMatching.use_gpu".into(),
                "1".into(),
                "--FeatureMatching.gpu_index".into(),
                "0".into(),
            ],
            StageOutput::Database(workspace.join("database.db")),
        ),
        stage(
            colmap,
            "sparse-mapping",
            "CPU sparse mapping and bundle adjustment",
            "03-mapper.log",
            vec![
                "mapper".into(),
                "--database_path".into(),
                database,
                "--image_path".into(),
                images,
                "--output_path".into(),
                sparse,
                "--Mapper.random_seed".into(),
                "0".into(),
                "--Mapper.ba_use_gpu".into(),
                "0".into(),
                "--Mapper.ba_local_max_num_iterations".into(),
                "16".into(),
                "--Mapper.ba_global_max_num_iterations".into(),
                "33".into(),
                "--Mapper.ba_global_frames_ratio".into(),
                "1.21".into(),
                "--Mapper.ba_global_points_ratio".into(),
                "1.21".into(),
                "--Mapper.ba_global_max_refinements".into(),
                "2".into(),
            ],
            StageOutput::SparseModel(workspace.to_path_buf()),
        ),
    ]
}

pub fn dense_stages(
    colmap: &Path,
    images: &Path,
    workspace: &Path,
    models: &[PathBuf],
) -> Result<Vec<Stage>, String> {
    let sparse_root = workspace.join("sparse");
    let mut numbered = Vec::with_capacity(models.len());
    for model in models {
        if model.parent() != Some(sparse_root.as_path()) {
            return Err(format!(
                "Sparse model is outside the workspace: {}",
                model.display()
            ));
        }
        let id = model
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("Invalid sparse model path: {}", model.display()))?;
        let number = id
            .parse::<u32>()
            .map_err(|_| format!("Sparse model folder is not numeric: {}", model.display()))?;
        numbered.push((number, model.clone()));
    }
    numbered.sort_by_key(|(number, _)| *number);

    let image_path = images.as_os_str().to_os_string();
    let mut stages = Vec::with_capacity(numbered.len() * 3);
    for (number, model) in numbered {
        let dense = workspace.join("dense").join(number.to_string());
        let model_path = model.into_os_string();
        let dense_path = dense.as_os_str().to_os_string();
        let fused = dense.join("fused.ply").into_os_string();
        stages.push(stage(
            colmap,
            format!("model-{number}-undistort"),
            &format!("Model {number}: image undistortion"),
            &format!("10-model-{number}-undistort.log"),
            vec![
                "image_undistorter".into(),
                "--image_path".into(),
                image_path.clone(),
                "--input_path".into(),
                model_path,
                "--output_path".into(),
                dense_path.clone(),
                "--output_type".into(),
                "COLMAP".into(),
                "--max_image_size".into(),
                "1600".into(),
                "--num_patch_match_src_images".into(),
                "20".into(),
            ],
            StageOutput::Undistorted(dense.clone()),
        ));
        stages.push(stage(
            colmap,
            format!("model-{number}-patch-match"),
            &format!("Model {number}: CUDA PatchMatch stereo"),
            &format!("11-model-{number}-patch-match.log"),
            vec![
                "patch_match_stereo".into(),
                "--workspace_path".into(),
                dense_path.clone(),
                "--workspace_format".into(),
                "COLMAP".into(),
                "--PatchMatchStereo.gpu_index".into(),
                "0".into(),
                "--PatchMatchStereo.max_image_size".into(),
                "1600".into(),
                "--PatchMatchStereo.geom_consistency".into(),
                "0".into(),
                "--PatchMatchStereo.filter".into(),
                "1".into(),
                "--PatchMatchStereo.window_radius".into(),
                "4".into(),
                "--PatchMatchStereo.window_step".into(),
                "2".into(),
                "--PatchMatchStereo.num_samples".into(),
                "10".into(),
                "--PatchMatchStereo.num_iterations".into(),
                "5".into(),
            ],
            StageOutput::PatchMatch(dense.clone()),
        ));
        stages.push(stage(
            colmap,
            format!("model-{number}-fusion"),
            &format!("Model {number}: dense point fusion"),
            &format!("12-model-{number}-fusion.log"),
            vec![
                "stereo_fusion".into(),
                "--workspace_path".into(),
                dense_path,
                "--workspace_format".into(),
                "COLMAP".into(),
                "--input_type".into(),
                "photometric".into(),
                "--output_path".into(),
                fused,
                "--StereoFusion.max_image_size".into(),
                "1600".into(),
                "--StereoFusion.check_num_images".into(),
                "33".into(),
            ],
            StageOutput::Fused(dense.join("fused.ply")),
        ));
    }
    Ok(stages)
}

pub fn run(
    colmap: &Path,
    images: &Path,
    workspace: &Path,
    cancel: Arc<AtomicBool>,
    events: SyncSender<String>,
) -> Result<PipelineOutcome, String> {
    let (control, _ignored) = std::sync::mpsc::channel();
    run_with_events(colmap, images, workspace, cancel, events, control)
}

pub fn run_with_events(
    colmap: &Path,
    images: &Path,
    workspace: &Path,
    cancel: Arc<AtomicBool>,
    events: SyncSender<String>,
    control: Sender<PipelineEvent>,
) -> Result<PipelineOutcome, String> {
    let mut project = Project::open_or_create(workspace, images, colmap, PipelineId::RtxDense)?;
    std::fs::create_dir_all(workspace.join("sparse")).map_err(|e| e.to_string())?;
    let log = workspace.join("run.log");
    for mut stage in sparse_stages(colmap, images, workspace) {
        let outcome = run_stage(
            &mut stage,
            workspace,
            &log,
            &mut project,
            cancel.clone(),
            events.clone(),
            &control,
        )?;
        if outcome == job::Outcome::Cancelled || cancel.load(Ordering::SeqCst) {
            return Ok(PipelineOutcome::Cancelled);
        }
    }

    let models = sparse_models(workspace)?;
    std::fs::create_dir_all(workspace.join("dense")).map_err(|e| e.to_string())?;
    for model in &models {
        let id = model.file_name().ok_or("Sparse model has no folder name")?;
        std::fs::create_dir_all(workspace.join("dense").join(id)).map_err(|e| e.to_string())?;
    }
    for mut stage in dense_stages(colmap, images, workspace, &models)? {
        let outcome = run_stage(
            &mut stage,
            workspace,
            &log,
            &mut project,
            cancel.clone(),
            events.clone(),
            &control,
        )?;
        if outcome == job::Outcome::Cancelled || cancel.load(Ordering::SeqCst) {
            return Ok(PipelineOutcome::Cancelled);
        }
    }

    let mut fused_clouds = Vec::with_capacity(models.len());
    for model in &models {
        let fused = workspace
            .join("dense")
            .join(model.file_name().unwrap())
            .join("fused.ply");
        if cancel.load(Ordering::SeqCst) {
            return Ok(PipelineOutcome::Cancelled);
        }
        let _ = events.try_send(format!("Validating dense output {}", fused.display()));
        validate_fused_cloud(&fused)?;
        if cancel.load(Ordering::SeqCst) {
            return Ok(PipelineOutcome::Cancelled);
        }
        fused_clouds.push(fused);
    }
    Ok(PipelineOutcome::Completed(PipelineResult {
        models,
        fused_clouds,
    }))
}

fn run_stage(
    stage: &mut Stage,
    workspace: &Path,
    log: &Path,
    project: &mut Project,
    cancel: Arc<AtomicBool>,
    events: SyncSender<String>,
    control: &Sender<PipelineEvent>,
) -> Result<job::Outcome, String> {
    stage.command.current_dir(workspace);
    let name = stage.name.clone();
    let fingerprint = project.stage_fingerprint(&stage.command);
    if project.stage_complete(&stage.id, &fingerprint)? {
        stage.output.validate().map_err(|error| {
            format!(
                "Saved stage '{}' is marked complete but its output is invalid: {error}",
                stage.name
            )
        })?;
        let _ = events.try_send(format!(
            "Resuming project — reusing completed stage: {name}"
        ));
        let _ = control.send(PipelineEvent::Cached(name));
        return Ok(job::Outcome::Completed);
    }

    project.set_stage(&stage.id, &fingerprint, StageState::Running)?;
    let _ = control.send(PipelineEvent::Started(name.clone()));
    let _ = events.try_send(format!("RTX pipeline — {name}"));
    let executed = if log.is_file() {
        job::execute_append(&mut stage.command, log, cancel, events)
    } else {
        job::execute(&mut stage.command, log, cancel, events)
    };
    let outcome = match executed {
        Ok(outcome) => outcome,
        Err(error) => {
            let manifest = project.set_stage(&stage.id, &fingerprint, StageState::Failed);
            let _ = control.send(PipelineEvent::Failed(name));
            return match manifest {
                Ok(()) => Err(error),
                Err(manifest) => Err(format!("{error}; additionally, {manifest}")),
            };
        }
    };
    if outcome == job::Outcome::Cancelled {
        project.set_stage(&stage.id, &fingerprint, StageState::Cancelled)?;
        let _ = control.send(PipelineEvent::Cancelled(name));
        return Ok(outcome);
    }
    if let Err(error) = stage.output.validate() {
        project.set_stage(&stage.id, &fingerprint, StageState::Failed)?;
        let _ = control.send(PipelineEvent::Failed(name.clone()));
        return Err(format!("Stage '{name}' produced invalid output: {error}"));
    }
    project.set_stage(&stage.id, &fingerprint, StageState::Complete)?;
    let _ = control.send(PipelineEvent::Finished(name));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    fn args(s: &Stage) -> Vec<String> {
        s.command
            .get_args()
            .map(|x| x.to_string_lossy().into_owned())
            .collect()
    }
    #[test]
    fn rejects_nonempty_malformed_dense_output() {
        let path = std::env::temp_dir().join(format!("bad-fused-{}.ply", std::process::id()));
        std::fs::write(&path, b"not a ply").unwrap();
        let result = validate_fused_cloud(&path);
        let _ = std::fs::remove_file(path);
        assert!(result.is_err());
    }

    #[test]
    fn builds_exact_medium_sparse_gpu_then_cpu_mapper_stages() {
        let stages = sparse_stages(
            Path::new("C:/COLMAP/bin/colmap.exe"),
            Path::new("C:/photos & more"),
            Path::new("C:/workspace"),
        );
        assert_eq!(stages.len(), 3);
        assert_eq!(
            stages
                .iter()
                .map(|s| s.log_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "01-feature-extractor.log",
                "02-exhaustive-matcher.log",
                "03-mapper.log"
            ]
        );
        let feature = args(&stages[0]);
        assert_eq!(feature[0], "feature_extractor");
        assert_eq!(feature[2].replace('\\', "/"), "C:/workspace/database.db");
        assert_eq!(
            &feature[3..],
            &[
                "--image_path",
                "C:/photos & more",
                "--FeatureExtraction.type",
                "SIFT",
                "--FeatureExtraction.use_gpu",
                "1",
                "--FeatureExtraction.gpu_index",
                "0",
                "--FeatureExtraction.max_image_size",
                "1600",
                "--SiftExtraction.max_num_features",
                "4096"
            ]
        );
        assert!(
            args(&stages[1])
                .windows(2)
                .any(|w| w == ["--FeatureMatching.use_gpu", "1"])
        );
        let mapper = args(&stages[2]);
        assert!(mapper.windows(2).any(|w| w == ["--Mapper.ba_use_gpu", "0"]));
        assert!(
            mapper
                .windows(2)
                .any(|w| w == ["--Mapper.ba_global_max_refinements", "2"])
        );
        assert!(
            stages
                .iter()
                .all(|s| s.command.get_envs().any(|(k, _)| k == "QT_PLUGIN_PATH"))
        );
    }

    #[test]
    fn preserves_non_unicode_windows_paths_in_command_arguments() {
        use std::os::windows::ffi::OsStringExt;

        let unusual = OsString::from_wide(&[0xD800, b'x' as u16]);
        let images = PathBuf::from(&unusual);
        let workspace = PathBuf::from("C:/workspace").join(&unusual);
        let expected_database = workspace.join("database.db");
        let stages = sparse_stages(Path::new("C:/COLMAP/bin/colmap.exe"), &images, &workspace);
        let arguments: Vec<_> = stages[0].command.get_args().collect();
        assert!(
            arguments
                .iter()
                .any(|argument| *argument == images.as_os_str())
        );
        assert!(
            arguments
                .iter()
                .any(|argument| *argument == expected_database.as_os_str())
        );
    }
    #[test]
    fn schedules_dense_stages_only_for_numbered_model_directories() {
        let base = std::env::temp_dir().join(format!("rtx-stage-{}", std::process::id()));
        let sparse = base.join("sparse");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(sparse.join("1")).unwrap();
        std::fs::create_dir_all(sparse.join("0")).unwrap();
        std::fs::write(sparse.join("project.ini"), b"x").unwrap();
        let models = vec![sparse.join("1"), sparse.join("0")];
        let stages = dense_stages(
            Path::new("C:/COLMAP/bin/colmap.exe"),
            Path::new("C:/photos"),
            &base,
            &models,
        )
        .unwrap();
        assert_eq!(stages.len(), 6);
        assert_eq!(
            stages
                .iter()
                .map(|s| s.log_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "10-model-0-undistort.log",
                "11-model-0-patch-match.log",
                "12-model-0-fusion.log",
                "10-model-1-undistort.log",
                "11-model-1-patch-match.log",
                "12-model-1-fusion.log"
            ]
        );
        let patch = args(&stages[1]);
        assert!(
            patch
                .windows(2)
                .any(|w| w == ["--PatchMatchStereo.gpu_index", "0"])
        );
        assert!(
            patch
                .windows(2)
                .any(|w| w == ["--PatchMatchStereo.max_image_size", "1600"])
        );
        assert!(
            patch
                .windows(2)
                .any(|w| w == ["--PatchMatchStereo.geom_consistency", "0"])
        );
        let fusion = args(&stages[2]);
        assert!(
            fusion
                .windows(2)
                .any(|w| w == ["--input_type", "photometric"])
        );
        assert!(
            fusion
                .iter()
                .any(|x| x.replace('\\', "/").ends_with("dense/0/fused.ply"))
        );
        std::fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn rejects_model_outside_workspace_or_nonnumeric_name() {
        let w = Path::new("C:/work");
        assert!(
            dense_stages(
                Path::new("C:/c.exe"),
                Path::new("C:/p"),
                w,
                &[PathBuf::from("C:/other/0")]
            )
            .is_err()
        );
        assert!(
            dense_stages(
                Path::new("C:/c.exe"),
                Path::new("C:/p"),
                w,
                &[PathBuf::from("C:/work/sparse/no")]
            )
            .is_err()
        );
    }

    #[test]
    fn completed_valid_stage_is_reused_without_touching_log() {
        let base = std::env::temp_dir().join(format!(
            "rtx-resume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let images = base.join("images");
        let workspace = base.join("workspace");
        let colmap = base.join("colmap.exe");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(images.join("one.jpg"), b"photo").unwrap();
        std::fs::write(&colmap, b"engine").unwrap();
        let database = workspace.join("database.db");
        std::fs::write(&database, b"valid database fixture").unwrap();
        let mut project =
            Project::open_or_create(&workspace, &images, &colmap, PipelineId::RtxDense).unwrap();
        let make_stage = || {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/C", "exit /b 0"]);
            Stage {
                id: "test-stage".into(),
                name: "Test stage".into(),
                log_name: "test.log".into(),
                command,
                output: StageOutput::Database(database.clone()),
            }
        };
        let log = workspace.join("run.log");
        let cancel = Arc::new(AtomicBool::new(false));
        let (events_tx, _events_rx) = mpsc::sync_channel(16);
        let (control_tx, control_rx) = mpsc::channel();
        let mut first = make_stage();
        assert_eq!(
            run_stage(
                &mut first,
                &workspace,
                &log,
                &mut project,
                cancel.clone(),
                events_tx.clone(),
                &control_tx,
            )
            .unwrap(),
            job::Outcome::Completed
        );
        let first_len = std::fs::metadata(&log).unwrap().len();
        while control_rx.try_recv().is_ok() {}

        let mut second = make_stage();
        assert_eq!(
            run_stage(
                &mut second,
                &workspace,
                &log,
                &mut project,
                cancel,
                events_tx,
                &control_tx,
            )
            .unwrap(),
            job::Outcome::Completed
        );
        assert_eq!(std::fs::metadata(&log).unwrap().len(), first_len);
        assert!(matches!(
            control_rx.recv().unwrap(),
            PipelineEvent::Cached(_)
        ));
        std::fs::remove_dir_all(base).unwrap();
    }
}
