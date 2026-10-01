#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use colmap_launcher::{
    claim_workspace, diagnostics, job, metrics, orbit, ply, point_cloud, preflight,
    reconstruction_command, rtx_pipeline, settings, sparse_models,
    studio_support::{
        PipelineEvent, PipelineKind, StageStatus, StageView, apply_stage_event, initial_stages,
    },
    validate_inputs,
};
use eframe::egui::{self, Color32, RichText, Vec2};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

enum JobResult {
    SparseSuccess {
        models: Vec<PathBuf>,
        metrics: Option<metrics::ModelMetrics>,
        metrics_error: Option<String>,
    },
    DenseSuccess {
        models: Vec<PathBuf>,
        fused_clouds: Vec<PathBuf>,
        metrics: Option<metrics::ModelMetrics>,
        metrics_error: Option<String>,
    },
    Cancelled,
}

struct RunSummary {
    pipeline: PipelineKind,
    model_count: usize,
    metrics: Option<metrics::ModelMetrics>,
    metrics_error: Option<String>,
    dense_points: Option<u64>,
    dense_bytes: Option<u64>,
    elapsed: Duration,
    workspace: PathBuf,
}
struct Studio {
    install: String,
    images: String,
    workspace: String,
    output_parent: String,
    project_name: String,
    status: String,
    worker: Option<JoinHandle<Result<JobResult, String>>>,
    events: Option<mpsc::Receiver<String>>,
    stage_events: Option<mpsc::Receiver<PipelineEvent>>,
    stages: Vec<StageView>,
    cancel: Arc<AtomicBool>,
    logs: VecDeque<String>,
    started: Option<Instant>,
    elapsed: Duration,
    cloud: Option<point_cloud::Cloud>,
    model_path: Option<PathBuf>,
    sparse_output: Option<PathBuf>,
    loader: Option<JoinHandle<Result<point_cloud::Cloud, String>>>,
    yaw: f32,
    pitch: f32,
    zoom: f32,
    pan: Vec2,
    point_size: f32,
    projected_cache: Vec<([f32; 3], [u8; 3])>,
    projection_dirty: bool,
    pipeline: PipelineKind,
    dense_output: Option<PathBuf>,
    summary: Option<RunSummary>,
    show_technical_log: bool,
    settings: settings::Settings,
    settings_save_allowed: bool,
    diagnostics_worker: Option<JoinHandle<diagnostics::DiagnosticReport>>,
    diagnostics: Option<diagnostics::DiagnosticReport>,
    closing: bool,
}
impl Default for Studio {
    fn default() -> Self {
        let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default());
        let samples = home.join("Documents").join("COLMAP Tests");
        let initial_workspace = fresh_workspace(&samples);
        let loaded_settings = settings::load_recovering(&settings::default_path());
        let settings_warning = loaded_settings.warning;
        let settings_save_allowed = loaded_settings.save_allowed;
        let saved = loaded_settings.settings;
        let install = if saved.install.as_os_str().is_empty() {
            home.join("Desktop").join("colmap")
        } else {
            saved.install.clone()
        };
        Self {
            install: install.display().to_string(),
            images: samples
                .join("south-building")
                .join("south-building")
                .join("images")
                .display()
                .to_string(),
            workspace: initial_workspace.display().to_string(),
            output_parent: samples.display().to_string(),
            project_name: initial_workspace
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("new-project")
                .to_owned(),
            status: settings_warning
                .unwrap_or_else(|| "Ready — choose photos or open an existing model".into()),
            worker: None,
            events: None,
            stage_events: None,
            stages: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            logs: VecDeque::new(),
            started: None,
            elapsed: Duration::ZERO,
            cloud: None,
            model_path: None,
            sparse_output: None,
            loader: None,
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            pan: Vec2::ZERO,
            point_size: 1.4,
            projected_cache: Vec::new(),
            projection_dirty: true,
            pipeline: PipelineKind::QuickSparse,
            dense_output: None,
            summary: None,
            show_technical_log: false,
            settings: saved,
            settings_save_allowed,
            diagnostics_worker: None,
            diagnostics: None,
            closing: false,
        }
    }
}
fn fresh_workspace(parent: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    parent.join(format!("studio-run-{stamp}"))
}
fn project_folder(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let trimmed = name.trim();
    let basename = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(basename.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (basename.len() == 4
            && (basename.starts_with("COM") || basename.starts_with("LPT"))
            && matches!(basename.as_bytes()[3], b'1'..=b'9'));
    if trimmed.is_empty()
        || trimmed != name
        || trimmed == "."
        || trimmed == ".."
        || trimmed.ends_with('.')
        || reserved
        || trimmed.chars().any(|c| {
            c.is_ascii_control()
                || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return Err(
            "Project name is empty, reserved, or contains a Windows-invalid character".into(),
        );
    }
    Ok(parent.join(trimmed))
}

fn folder_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(RichText::new(label).strong());
    ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
    if ui.button(format!("Browse {label}…")).clicked() {
        let mut picker = rfd::FileDialog::new().set_title(label);
        if Path::new(value).is_dir() {
            picker = picker.set_directory(&*value);
        }
        if let Some(path) = picker.pick_folder() {
            *value = path.display().to_string();
        }
    }
    ui.add_space(12.0);
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else {
        format!("{bytes} bytes")
    }
}

fn pipeline_card(ui: &mut egui::Ui, pipeline: PipelineKind, selected: PipelineKind) -> bool {
    let marker = if pipeline == PipelineKind::QuickSparse {
        "⚡"
    } else {
        "◆"
    };
    let text = format!(
        "{marker}  {}\n{}\n{}",
        pipeline.title(),
        pipeline.summary(),
        pipeline.output()
    );
    ui.add_sized(
        [ui.available_width(), 72.0],
        egui::Button::new(RichText::new(text).size(12.5)).selected(pipeline == selected),
    )
    .on_hover_text(if pipeline == PipelineKind::QuickSparse {
        "Fast preview. Dense processing is not run."
    } else {
        "Runs CUDA PatchMatch and produces a detailed fused PLY point cloud."
    })
    .clicked()
}

impl Studio {
    fn start(&mut self) {
        if self.worker.is_some() || self.loader.is_some() {
            return;
        }
        let requested_workspace =
            match project_folder(Path::new(&self.output_parent), &self.project_name) {
                Ok(path) => path,
                Err(error) => {
                    self.status = format!("Error: {error}");
                    return;
                }
            };
        let exe = PathBuf::from(&self.install).join("bin").join("colmap.exe");
        let paths = std::path::absolute(exe).and_then(|e| {
            Ok((
                e,
                std::path::absolute(&self.images)?,
                std::path::absolute(requested_workspace)?,
            ))
        });
        let (exe, images, workspace) = match paths {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("Error: {e}");
                return;
            }
        };
        self.workspace = workspace.display().to_string();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let (tx, rx) = mpsc::sync_channel(512);
        let (stage_tx, stage_rx) = mpsc::channel();
        self.events = Some(rx);
        self.stage_events = Some(stage_rx);
        self.stages = initial_stages(self.pipeline);
        self.logs.clear();
        self.cloud = None;
        self.model_path = None;
        self.sparse_output = None;
        self.dense_output = None;
        self.summary = None;
        self.started = Some(Instant::now());
        self.elapsed = Duration::ZERO;
        self.status = "Running — preparing reconstruction".into();
        let pipeline = self.pipeline;
        self.worker = Some(std::thread::spawn(move || {
            validate_inputs(&exe, &images, &workspace)?;
            if cancel.load(Ordering::SeqCst) {
                return Ok(JobResult::Cancelled);
            }
            if pipeline == PipelineKind::RtxDense {
                let check = preflight::inspect(&images, &workspace)?;
                let _ = tx.try_send(format!(
                    "Dense preflight: {} photos, {} source, about {} working space required, {} available",
                    check.image_count,
                    format_bytes(check.source_bytes),
                    format_bytes(check.estimated_required_bytes),
                    format_bytes(check.available_bytes)
                ));
                if !check.has_enough_space() {
                    return Err(format!(
                        "Dense preflight needs about {} free, but only {} is available",
                        format_bytes(check.estimated_required_bytes),
                        format_bytes(check.available_bytes)
                    ));
                }
            }
            let _workspace_claim = claim_workspace(&workspace)?;
            if pipeline == PipelineKind::RtxDense {
                return match rtx_pipeline::run_with_events(
                    &exe,
                    &images,
                    &workspace,
                    cancel.clone(),
                    tx.clone(),
                    stage_tx,
                )? {
                    rtx_pipeline::PipelineOutcome::Completed(result) => {
                        let (analyzed, metrics_error) = if let Some(model) = result.models.first() {
                            match metrics::analyze(&exe, model, cancel.clone()) {
                                Ok(metrics::AnalysisOutcome::Completed(metrics)) => {
                                    (Some(metrics), None)
                                }
                                Ok(metrics::AnalysisOutcome::Cancelled) => {
                                    return Ok(JobResult::Cancelled);
                                }
                                Err(error) => {
                                    let _ = tx.try_send(format!(
                                        "Metrics warning: model analysis was unavailable: {error}"
                                    ));
                                    (None, Some(error))
                                }
                            }
                        } else {
                            (None, None)
                        };
                        Ok(JobResult::DenseSuccess {
                            models: result.models,
                            fused_clouds: result.fused_clouds,
                            metrics: analyzed,
                            metrics_error,
                        })
                    }
                    rtx_pipeline::PipelineOutcome::Cancelled => Ok(JobResult::Cancelled),
                };
            }
            let stage = pipeline.stages()[0].to_owned();
            let _ = stage_tx.send(PipelineEvent::Started(stage.clone()));
            let mut command = reconstruction_command(&exe, &images, &workspace);
            command.current_dir(&workspace);
            let result = match job::execute(
                &mut command,
                &workspace.join("run.log"),
                cancel.clone(),
                tx.clone(),
            ) {
                Ok(result) => result,
                Err(error) => {
                    let _ = stage_tx.send(PipelineEvent::Failed(stage));
                    return Err(error);
                }
            };
            if result == job::Outcome::Cancelled || cancel.load(Ordering::SeqCst) {
                let _ = stage_tx.send(PipelineEvent::Cancelled(stage));
                return Ok(JobResult::Cancelled);
            }
            let models = match sparse_models(&workspace) {
                Ok(models) => models,
                Err(error) => {
                    let _ = stage_tx.send(PipelineEvent::Failed(stage));
                    return Err(error);
                }
            };
            let _ = stage_tx.send(PipelineEvent::Finished(stage));
            let (analyzed, metrics_error) = if let Some(model) = models.first() {
                match metrics::analyze(&exe, model, cancel) {
                    Ok(metrics::AnalysisOutcome::Completed(metrics)) => (Some(metrics), None),
                    Ok(metrics::AnalysisOutcome::Cancelled) => {
                        return Ok(JobResult::Cancelled);
                    }
                    Err(error) => {
                        let _ = tx.try_send(format!(
                            "Metrics warning: model analysis was unavailable: {error}"
                        ));
                        (None, Some(error))
                    }
                }
            } else {
                (None, None)
            };
            Ok(JobResult::SparseSuccess {
                models,
                metrics: analyzed,
                metrics_error,
            })
        }));
    }
    fn run_diagnostics(&mut self) {
        if self.diagnostics_worker.is_some() {
            return;
        }
        let install = PathBuf::from(&self.install);
        let workspace = PathBuf::from(&self.workspace);
        self.diagnostics = None;
        self.status = "Checking COLMAP, GPU, VRAM, and disk…".into();
        self.diagnostics_worker = Some(std::thread::spawn(move || {
            diagnostics::run(&install, &workspace)
        }));
    }
    fn cancel(&mut self) {
        if self.worker.is_some() {
            self.cancel.store(true, Ordering::SeqCst);
            self.status = "Cancelling — stopping COLMAP; partial files will be kept".into();
        }
    }
    fn load_model(&mut self, path: PathBuf) {
        if self.loader.is_some() || self.worker.is_some() {
            return;
        }
        let file = if path.is_dir() {
            path.join("points3D.bin")
        } else {
            path
        };
        self.model_path = Some(file.clone());
        self.cloud = None;
        self.status = if self.summary.is_some() {
            "Complete — output saved; loading point-cloud preview…".into()
        } else {
            "Loading point cloud…".into()
        };
        self.loader = Some(std::thread::spawn(move || {
            if file
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ply"))
            {
                ply::load(&file)
            } else {
                point_cloud::load(&file)
            }
        }));
    }
    fn poll(&mut self) {
        if self
            .diagnostics_worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            let report = self
                .diagnostics_worker
                .take()
                .expect("checked above")
                .join()
                .ok();
            if let Some(report) = report {
                self.status = if report.quick_ready() {
                    "Compatibility check finished — COLMAP detected; review GPU and disk facts below".into()
                } else {
                    "Compatibility check found a COLMAP installation problem".into()
                };
                self.diagnostics = Some(report);
            } else {
                self.status = "Error: compatibility check worker failed".into();
            }
        }
        if let Some(rx) = &self.events {
            for line in rx.try_iter().take(1000) {
                if self.logs.len() >= 800 {
                    self.logs.pop_front();
                }
                self.logs.push_back(line);
            }
        }
        if let Some(rx) = &self.stage_events {
            let control: Vec<_> = rx.try_iter().collect();
            for event in control {
                if let PipelineEvent::Started(label) = &event {
                    self.status = format!("Running — {label}");
                }
                apply_stage_event(&mut self.stages, event);
            }
        }
        if self.worker.is_some() {
            self.elapsed = self.started.map(|s| s.elapsed()).unwrap_or_default();
        }
        if self.worker.as_ref().is_some_and(|w| w.is_finished()) {
            match self
                .worker
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err("Background worker panicked".into()))
            {
                Ok(JobResult::SparseSuccess {
                    models,
                    metrics,
                    metrics_error,
                }) => {
                    self.status = format!("Complete — {} sparse model(s) saved", models.len());
                    self.summary = Some(RunSummary {
                        pipeline: PipelineKind::QuickSparse,
                        model_count: models.len(),
                        metrics,
                        metrics_error,
                        dense_points: None,
                        dense_bytes: None,
                        elapsed: self.elapsed,
                        workspace: PathBuf::from(&self.workspace),
                    });
                    self.record_recent_project(PipelineKind::QuickSparse);
                    if let Some(model) = models.first() {
                        let sparse = model.join("points3D.bin");
                        self.sparse_output = Some(sparse.clone());
                        self.load_model(sparse);
                    }
                }
                Ok(JobResult::DenseSuccess {
                    models,
                    fused_clouds,
                    metrics,
                    metrics_error,
                }) => {
                    self.dense_output = fused_clouds.first().cloned();
                    self.sparse_output = models.first().map(|model| model.join("points3D.bin"));
                    let dense_points = fused_clouds
                        .first()
                        .and_then(|path| ply::vertex_count(path).ok());
                    let dense_bytes = fused_clouds
                        .first()
                        .and_then(|path| std::fs::metadata(path).ok())
                        .map(|metadata| metadata.len());
                    self.summary = Some(RunSummary {
                        pipeline: PipelineKind::RtxDense,
                        model_count: models.len(),
                        metrics,
                        metrics_error,
                        dense_points,
                        dense_bytes,
                        elapsed: self.elapsed,
                        workspace: PathBuf::from(&self.workspace),
                    });
                    self.status = format!(
                        "Complete — {} sparse and {} dense model(s) saved",
                        models.len(),
                        fused_clouds.len()
                    );
                    self.record_recent_project(PipelineKind::RtxDense);
                    if let Some(dense) = self.dense_output.clone() {
                        self.load_model(dense);
                    }
                }
                Ok(JobResult::Cancelled) => {
                    self.status =
                        "Cancelled — partial output retained; use a new workspace to retry".into()
                }
                Err(e) => self.status = format!("Error: {e}"),
            }
        }
        if self.loader.as_ref().is_some_and(|w| w.is_finished()) {
            match self
                .loader
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err("Preview loader panicked".into()))
            {
                Ok(cloud) => {
                    let kind = if self.model_path.as_ref().is_some_and(|path| {
                        path.extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("ply"))
                    }) {
                        "dense fused"
                    } else {
                        "sparse"
                    };
                    let preview = format!(
                        "{} {kind} points ({} displayed)",
                        cloud.total_points,
                        cloud.points.len()
                    );
                    self.status = if self.summary.is_some() {
                        format!("Complete — output saved; preview ready — {preview}")
                    } else {
                        format!("Preview ready — {preview}")
                    };
                    self.cloud = Some(cloud);
                    self.projection_dirty = true;
                    self.reset_view();
                }
                Err(e) => {
                    self.status = if self.summary.is_some() {
                        format!("Complete — output saved; preview unavailable: {e}")
                    } else {
                        format!("Error loading preview: {e}")
                    }
                }
            }
        }
    }
    fn reset_view(&mut self) {
        self.yaw = 0.0;
        self.pitch = 0.0;
        self.zoom = 3.0;
        self.pan = Vec2::ZERO;
        self.projection_dirty = true;
    }
    fn record_recent_project(&mut self, pipeline: PipelineKind) {
        self.settings.install = PathBuf::from(&self.install);
        self.settings.record(settings::RecentProject {
            workspace: PathBuf::from(&self.workspace),
            images: PathBuf::from(&self.images),
            pipeline: if pipeline == PipelineKind::RtxDense {
                "dense"
            } else {
                "quick"
            }
            .into(),
            last_opened: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
        if !self.settings_save_allowed {
            self.logs.push_back(
                "Settings warning: existing invalid settings could not be preserved, so no settings were saved"
                    .into(),
            );
        } else if let Err(error) = settings::save_atomic(&settings::default_path(), &self.settings)
        {
            self.logs.push_back(format!("Settings warning: {error}"));
        }
    }
    fn reopen_recent(&mut self, project: settings::RecentProject) {
        self.workspace = project.workspace.display().to_string();
        self.output_parent = project
            .workspace
            .parent()
            .unwrap_or(Path::new("."))
            .display()
            .to_string();
        self.project_name = project
            .workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_owned();
        self.images = project.images.display().to_string();
        self.pipeline = if project.pipeline == "dense" {
            PipelineKind::RtxDense
        } else {
            PipelineKind::QuickSparse
        };
        self.summary = None;
        self.stages.clear();
        self.dense_output = Some(project.workspace.join("dense").join("0").join("fused.ply"))
            .filter(|p| p.is_file());
        self.sparse_output = Some(
            project
                .workspace
                .join("sparse")
                .join("0")
                .join("points3D.bin"),
        )
        .filter(|p| p.is_file());
        let preview = self
            .dense_output
            .clone()
            .or_else(|| self.sparse_output.clone());
        if let Some(path) = preview {
            self.load_model(path);
        } else {
            self.cloud = None;
            self.model_path = None;
            self.status = "Project reopened — no completed point cloud found".into();
        }
    }
    fn reveal_path(&mut self, path: PathBuf) {
        if let Err(e) = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn()
        {
            self.status = format!("Error revealing file: {e}");
        }
    }
    fn open_path(&mut self, program: &str, path: PathBuf) {
        if let Err(e) = std::process::Command::new(program).arg(path).spawn() {
            self.status = format!("Error opening file: {e}");
        }
    }
    fn draw(&mut self, ui: &mut egui::Ui) {
        let busy =
            self.worker.is_some() || self.loader.is_some() || self.diagnostics_worker.is_some();
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("COLMAP / RUST STUDIO")
                        .size(23.0)
                        .strong()
                        .color(Color32::from_rgb(124, 211, 230)),
                );
                ui.label(
                    RichText::new(
                        "  Local photogrammetry • sparse preview • optional dense output",
                    )
                    .color(Color32::GRAY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.show_technical_log {
                        "Hide technical log"
                    } else {
                        "Show technical log"
                    };
                    if ui
                        .button(label)
                        .on_hover_text("The complete output is always saved to run.log")
                        .clicked()
                    {
                        self.show_technical_log = !self.show_technical_log;
                    }
                });
            });
            ui.add_space(8.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if busy {
                    ui.spinner();
                }
                let color = if self.status.starts_with("Error") {
                    Color32::from_rgb(255, 130, 120)
                } else {
                    Color32::from_rgb(150, 215, 190)
                };
                ui.label(RichText::new(&self.status).color(color));
                if self.elapsed > Duration::ZERO {
                    ui.label(format!(" • {:.1}s", self.elapsed.as_secs_f64()));
                }
            });
        });
        egui::Panel::left("setup").default_size(330.0).min_size(300.0).resizable(true).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(12.0); ui.heading("Reconstruction"); ui.label("Your photos stay on this computer."); ui.add_space(18.0);
                ui.add_enabled_ui(!busy, |ui| {
                    ui.label(RichText::new("1  CHOOSE PHOTOS").strong().size(11.0));
                    folder_field(ui, "Photo folder", &mut self.images);
                    ui.add_space(8.0);
                    ui.label(RichText::new("2  NAME THE PROJECT").strong().size(11.0));
                    ui.label("Project name");
                    ui.add(egui::TextEdit::singleline(&mut self.project_name).desired_width(f32::INFINITY));
                    folder_field(ui, "Output parent folder", &mut self.output_parent);
                    match project_folder(Path::new(&self.output_parent), &self.project_name) {
                        Ok(path) => {
                            self.workspace = path.display().to_string();
                            ui.small(format!("Project folder: {}", path.display()));
                        }
                        Err(error) => {
                            ui.colored_label(Color32::from_rgb(255, 160, 120), error);
                        }
                    }
                    if ui.button("Suggest a unique project name").on_hover_text("Uses a timestamped name; no files are written yet").clicked() {
                        self.project_name = fresh_workspace(Path::new("."))
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("new-project")
                            .to_owned();
                    }
                    ui.small("The project folder must be new or empty. Existing projects and source photos are never overwritten.");
                    ui.add_space(8.0);
                    egui::CollapsingHeader::new("Advanced engine settings")
                        .default_open(false)
                        .show(ui, |ui| {
                            folder_field(ui, "COLMAP installation", &mut self.install);
                            ui.small("Change this only when COLMAP was installed elsewhere.");
                            if ui
                                .add_enabled(
                                    self.diagnostics_worker.is_none(),
                                    egui::Button::new(if self.diagnostics_worker.is_some() {
                                        "Checking compatibility…"
                                    } else {
                                        "Run compatibility check"
                                    }),
                                )
                                .clicked()
                            {
                                self.run_diagnostics();
                            }
                            if let Some(report) = self.diagnostics.clone() {
                                ui.separator();
                                ui.label(format!(
                                    "{} COLMAP {}",
                                    if report.quick_ready() { "✓" } else { "!" },
                                    report.colmap_version.as_deref().unwrap_or("not detected")
                                ));
                                ui.label(format!(
                                    "{} Qt plugins",
                                    if report.plugins_found { "✓ found" } else { "! missing" }
                                ));
                                match report.gpu {
                                    Ok(gpu) => {
                                        ui.label(format!("✓ {}", gpu.name));
                                        ui.label(format!("  {} MiB VRAM • driver {}", gpu.vram_mib, gpu.driver));
                                        ui.small("CUDA workload support is verified by an actual dense run, not inferred from the GPU name.");
                                    }
                                    Err(error) => { ui.colored_label(Color32::from_rgb(255, 160, 120), format!("! {error}")); }
                                }
                                match report.free_bytes {
                                    Ok(bytes) => { ui.label(format!("✓ {} free on project volume", format_bytes(bytes))); }
                                    Err(error) => { ui.colored_label(Color32::from_rgb(255, 160, 120), format!("! {error}")); }
                                }
                            }
                        });
                    ui.add_space(18.0); ui.separator();
                    ui.label(RichText::new("CHOOSE A PIPELINE").strong());
                    if pipeline_card(ui, PipelineKind::QuickSparse, self.pipeline) {
                        self.pipeline = PipelineKind::QuickSparse;
                    }
                    if pipeline_card(ui, PipelineKind::RtxDense, self.pipeline) {
                        self.pipeline = PipelineKind::RtxDense;
                    }
                    if self.pipeline == PipelineKind::RtxDense {
                        egui::Frame::group(ui.style())
                            .fill(Color32::from_rgb(49, 42, 25))
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.label(RichText::new("⚠ Dense processing can take many minutes and needs substantial disk space.").strong());
                                ui.small("CUDA: SIFT and PatchMatch on GPU 0 • CPU: mapping, bundle adjustment, and fusion • preflight requires at least about 10 GB free");
                            });
                    }
                    if !self.stages.is_empty() {
                        ui.add_space(8.0);
                        ui.label(RichText::new("RUN STAGES").strong().size(11.0));
                        for stage in &self.stages {
                            let (icon, color) = match stage.status {
                                StageStatus::Pending => ("○", Color32::GRAY),
                                StageStatus::Running => ("▶", Color32::from_rgb(124, 211, 230)),
                                StageStatus::Complete => ("✓", Color32::from_rgb(150, 215, 190)),
                                StageStatus::Failed => ("!", Color32::from_rgb(255, 130, 120)),
                                StageStatus::Cancelled => ("■", Color32::from_rgb(235, 180, 100)),
                                StageStatus::Skipped => ("–", Color32::DARK_GRAY),
                            };
                            ui.label(RichText::new(format!("{icon}  {}", stage.label)).color(color).size(11.0));
                        }
                    }
                    ui.add_space(14.0);
                    if ui.add_sized([ui.available_width(), 42.0], egui::Button::new(RichText::new("Start reconstruction").strong())).clicked() { self.start(); }
                });
                if ui.add_enabled(self.worker.is_some() && !self.cancel.load(Ordering::SeqCst), egui::Button::new("Cancel reconstruction")).clicked() { self.cancel(); }
                ui.small("Cancel force-stops this run and retains partial files.");
                if let Some(summary) = &self.summary {
                    let workspace = summary.workspace.clone();
                    let dense = self.dense_output.clone();
                    let sparse = self.sparse_output.clone();
                    ui.add_space(18.0);
                    ui.separator();
                    ui.label(RichText::new("RESULTS").strong());
                    ui.label(RichText::new(summary.pipeline.title()).color(Color32::from_rgb(124, 211, 230)));
                    egui::Grid::new("result_metrics").num_columns(2).show(ui, |ui| {
                        ui.label("Elapsed"); ui.label(format!("{:.1}s", summary.elapsed.as_secs_f64())); ui.end_row();
                        ui.label("Models"); ui.label(summary.model_count.to_string()); ui.end_row();
                        if let Some(metrics) = &summary.metrics {
                            let suffix = if summary.model_count > 1 { " (first model)" } else { "" };
                            ui.label(format!("Registered photos{suffix}")); ui.label(metrics.registered_images.map_or_else(|| "—".into(), |v| v.to_string())); ui.end_row();
                            ui.label(format!("Sparse points{suffix}")); ui.label(metrics.points.map_or_else(|| "—".into(), |v| v.to_string())); ui.end_row();
                            ui.label(format!("Reprojection error{suffix}")); ui.label(metrics.mean_reprojection_error_px.map_or_else(|| "—".into(), |v| format!("{v:.3}px"))); ui.end_row();
                        }
                        if summary.metrics.is_none() && summary.metrics_error.is_some() {
                            ui.label("Model metrics"); ui.colored_label(Color32::from_rgb(255, 180, 100), "Unavailable — see technical log"); ui.end_row();
                        }
                        if let Some(points) = summary.dense_points { ui.label(if summary.model_count > 1 { "Dense points (first model)" } else { "Dense points" }); ui.label(points.to_string()); ui.end_row(); }
                        if let Some(bytes) = summary.dense_bytes { ui.label(if summary.model_count > 1 { "Dense file (first model)" } else { "Dense file" }); ui.label(format_bytes(bytes)); ui.end_row(); }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if let Some(path) = dense.clone()
                            && ui.button("Preview dense").clicked() { self.load_model(path); }
                        if let Some(path) = sparse.clone()
                            && ui.button("Preview sparse").clicked() { self.load_model(path); }
                        if ui.button("Open output").clicked() { self.open_path("explorer.exe", workspace.clone()); }
                        if let Some(path) = dense.clone()
                            && ui.button("Reveal PLY").clicked() { self.reveal_path(path); }
                        if let Some(path) = dense
                            && ui.button("Copy PLY path").clicked() { ui.ctx().copy_text(path.display().to_string()); }
                    });
                    if ui.button("Start another project").clicked() {
                        self.output_parent = workspace.parent().unwrap_or(Path::new(".")).display().to_string();
                        self.project_name = fresh_workspace(Path::new("."))
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("new-project")
                            .to_owned();
                        self.workspace = project_folder(Path::new(&self.output_parent), &self.project_name)
                            .unwrap_or_else(|_| fresh_workspace(Path::new(&self.output_parent)))
                            .display()
                            .to_string();
                        self.summary = None;
                        self.stages.clear();
                        self.cloud = None;
                        self.model_path = None;
                    }
                }
                ui.add_space(18.0); ui.separator();
                let recent = self.settings.recent.clone();
                if !recent.is_empty() {
                    egui::CollapsingHeader::new("Recent projects")
                        .default_open(true)
                        .show(ui, |ui| {
                            for project in recent {
                                let exists = project.workspace.is_dir();
                                let name = project.workspace.file_name().and_then(|n| n.to_str()).unwrap_or("Project");
                                let label = if exists { name.to_owned() } else { format!("{name} — missing") };
                                if ui.add_enabled(exists && !busy, egui::Button::new(label))
                                    .on_hover_text(project.workspace.display().to_string())
                                    .clicked()
                                {
                                    self.reopen_recent(project);
                                }
                            }
                        });
                    ui.add_space(8.0);
                }
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button("Open existing point cloud…").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .set_title("Select points3D.bin or fused.ply")
                            .add_filter("Point clouds", &["bin", "ply"])
                            .pick_file()
                    {
                        self.load_model(path);
                    }
                });
                if ui.add_enabled(Path::new(&self.workspace).is_dir(), egui::Button::new("Open workspace")).clicked() { self.open_path("explorer.exe", PathBuf::from(&self.workspace)); }
                let log = PathBuf::from(&self.workspace).join("run.log");
                if ui.add_enabled(log.is_file(), egui::Button::new("Open full log")).clicked() { self.open_path("notepad.exe", log); }
                ui.add_space(14.0); ui.small("Preview is a point cloud, not a solid or textured mesh. COLMAP computes; Rust manages the workflow.");
            });
        });
        if self.show_technical_log {
            egui::Panel::bottom("logs")
                .default_size(175.0)
                .min_size(80.0)
                .resizable(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Technical log");
                        ui.small("Recent lines • complete output in run.log");
                    });
                    egui::ScrollArea::both().stick_to_bottom(true).show_rows(
                        ui,
                        16.0,
                        self.logs.len(),
                        |ui, range| {
                            for index in range {
                                ui.label(RichText::new(&self.logs[index]).monospace().size(11.0));
                            }
                        },
                    );
                });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("3D point-cloud preview");
                if ui.button("Reset view").clicked() {
                    self.reset_view();
                }
                if let Some(path) = self.sparse_output.clone()
                    && ui
                        .button("Sparse")
                        .on_hover_text("Preview points3D.bin")
                        .clicked()
                {
                    self.load_model(path);
                }
                if let Some(path) = self.dense_output.clone()
                    && ui
                        .button("Dense")
                        .on_hover_text("Preview fused.ply")
                        .clicked()
                {
                    self.load_model(path);
                }
                ui.add(egui::Slider::new(&mut self.point_size, 0.5..=4.0).text("Point size"));
            });
            ui.small("Left-drag: orbit   •   Right-drag: pan   •   Scroll: zoom");
            if let Some(path) = &self.model_path {
                ui.label(RichText::new(path.display().to_string()).small());
            }
            self.paint_cloud(ui);
        });
    }
    fn paint_cloud(&mut self, ui: &mut egui::Ui) {
        let size = ui.available_size().max(Vec2::new(10.0, 10.0));
        let (response, painter) = ui.allocate_painter(size, egui::Sense::drag());
        let rect = response.rect;
        painter.rect_filled(rect, 6.0, Color32::from_rgb(14, 20, 29));
        let delta = ui.input(|i| i.pointer.delta());
        if response.dragged_by(egui::PointerButton::Primary) {
            self.yaw += delta.x * 0.008;
            self.pitch += delta.y * 0.008;
            self.projection_dirty = true;
        }
        if response.dragged_by(egui::PointerButton::Secondary) {
            self.pan += delta;
        }
        if response.hovered() {
            let old_zoom = self.zoom;
            self.zoom = (self.zoom * (ui.input(|i| i.smooth_scroll_delta.y) * 0.002).exp())
                .clamp(0.1, 10.0);
            self.projection_dirty |= self.zoom != old_zoom;
        }
        let center = rect.center() + self.pan;
        let scale = rect.width().min(rect.height()) * 0.85;
        if let Some(cloud) = &self.cloud {
            if self.projection_dirty {
                self.projected_cache.clear();
                self.projected_cache.extend(cloud.points.iter().map(|p| {
                    (
                        orbit::project(p.xyz, self.yaw, self.pitch, self.zoom),
                        p.rgb,
                    )
                }));
                self.projected_cache
                    .sort_unstable_by(|a, b| b.0[2].total_cmp(&a.0[2]));
                self.projection_dirty = false;
            }
            let mut mesh = egui::Mesh::default();
            for (p, rgb) in &self.projected_cache {
                let pos = center + Vec2::new(p[0] * scale, p[1] * scale);
                if rect.contains(pos) {
                    mesh.add_colored_rect(
                        egui::Rect::from_center_size(pos, Vec2::splat(self.point_size)),
                        Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
                    );
                }
            }
            painter.add(egui::Shape::mesh(mesh));
            painter.text(
                rect.left_top() + Vec2::splat(14.0),
                egui::Align2::LEFT_TOP,
                format!("{} points   •   zoom {:.1}×", cloud.total_points, self.zoom),
                egui::FontId::monospace(12.0),
                Color32::from_gray(180),
            );
        } else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No point cloud loaded\n\nBuild a reconstruction or open points3D.bin / fused.ply\nSparse and dense previews remain point clouds — not solid meshes",
                egui::FontId::proportional(18.0),
                Color32::from_gray(160),
            );
        }
    }
}
impl eframe::App for Studio {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.worker.is_some() || self.loader.is_some() || self.diagnostics_worker.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.worker.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.cancel();
        }
        if self.closing && self.worker.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}
impl Drop for Studio {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn main() -> eframe::Result {
    let model = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([900.0, 650.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "COLMAP Rust Studio",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            let mut app = Studio::default();
            if let Some(path) = model {
                app.load_model(path);
            }
            Ok(Box::new(app))
        }),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_preview_load_reports_error_and_clears_loader() {
        let mut app = Studio::default();
        app.load_model(PathBuf::from("Z:/missing/points3D.bin"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.loader.is_some() && Instant::now() < deadline {
            app.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.loader.is_none());
        assert!(app.cloud.is_none());
        assert!(app.status.starts_with("Error loading preview"));
    }
    #[test]
    fn preview_failure_does_not_replace_successful_reconstruction_status() {
        let mut app = Studio::default();
        app.summary = Some(RunSummary {
            pipeline: PipelineKind::QuickSparse,
            model_count: 1,
            metrics: None,
            metrics_error: None,
            dense_points: None,
            dense_bytes: None,
            elapsed: Duration::from_secs(2),
            workspace: PathBuf::from("C:/completed"),
        });
        app.load_model(PathBuf::from("Z:/missing/points3D.bin"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.loader.is_some() && Instant::now() < deadline {
            app.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.status.starts_with("Complete — output saved"));
        assert!(app.status.contains("preview unavailable"));
    }

    #[test]
    fn project_names_create_child_paths_and_reject_unsafe_names() {
        let parent = Path::new(r"C:\Projects");
        assert_eq!(
            project_folder(parent, "My scan").unwrap(),
            parent.join("My scan")
        );
        assert!(project_folder(parent, "../escape").is_err());
        assert!(project_folder(parent, " ").is_err());
        assert!(project_folder(parent, "CON").is_err());
        assert!(project_folder(parent, "scan.").is_err());
        assert!(project_folder(parent, "bad\u{1f}name").is_err());
    }

    #[test]
    fn start_rejects_invalid_project_name_without_using_stale_workspace() {
        let mut app = Studio::default();
        app.project_name = "CON".into();
        app.workspace = "C:/stale-valid-project".into();
        app.start();
        assert!(app.worker.is_none());
        assert!(app.status.starts_with("Error:"));
        assert_eq!(app.workspace, "C:/stale-valid-project");
    }

    #[test]
    fn reset_restores_orbit_pan_and_useful_zoom() {
        let mut app = Studio::default();
        app.yaw = 10.0;
        app.pitch = 3.0;
        app.pan = Vec2::splat(100.0);
        app.zoom = 9.0;
        app.reset_view();
        assert_eq!((app.yaw, app.pitch, app.zoom), (0.0, 0.0, 3.0));
        assert_eq!(app.pan, Vec2::ZERO);
    }
    #[test]
    fn start_rejects_invalid_inputs_without_creating_workspace() {
        let work = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("never-create-invalid-workspace");
        let mut app = Studio::default();
        app.install = "Z:/missing-colmap".into();
        app.images = "Z:/missing-photos".into();
        app.output_parent = work.parent().unwrap().display().to_string();
        app.project_name = work.file_name().unwrap().to_string_lossy().into_owned();
        app.workspace = work.to_string_lossy().into_owned();
        app.start();
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.worker.is_some() && Instant::now() < deadline {
            app.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.status.starts_with("Error:"));
        assert!(!work.exists());
    }
}
