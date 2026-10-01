use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

pub const FILE_NAME: &str = "project.json";
const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineId {
    QuickSparse,
    RtxDense,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageState {
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StageRecord {
    pub id: String,
    pub fingerprint: String,
    pub state: StageState,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProjectFile {
    format_version: u32,
    application_version: String,
    pipeline: PipelineId,
    images: String,
    colmap: String,
    input_fingerprint: String,
    created_at: u64,
    updated_at: u64,
    stages: Vec<StageRecord>,
}

pub struct Project {
    path: PathBuf,
    file: ProjectFile,
}

impl Project {
    pub fn open_or_create(
        workspace: &Path,
        images: &Path,
        colmap: &Path,
        pipeline: PipelineId,
    ) -> Result<Self, String> {
        let path = workspace.join(FILE_NAME);
        let input_fingerprint = input_fingerprint(images, colmap, pipeline)?;
        let images_text = absolute_text(images)?;
        let colmap_text = absolute_text(colmap)?;
        if path.exists() {
            let bytes = fs::read(&path).map_err(|error| format!("Cannot read project: {error}"))?;
            let file: ProjectFile = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Project file is invalid: {error}"))?;
            if file.format_version != FORMAT_VERSION {
                return Err(format!(
                    "Project format {} is unsupported; this build supports format {FORMAT_VERSION}",
                    file.format_version
                ));
            }
            if file.pipeline != pipeline {
                return Err(
                    "This project was created for a different reconstruction pipeline".into(),
                );
            }
            if file.images != images_text || file.colmap != colmap_text {
                return Err(
                    "This project belongs to different photos or a different COLMAP installation"
                        .into(),
                );
            }
            if file.input_fingerprint != input_fingerprint {
                return Err(
                    "The photos or COLMAP executable changed since this project was created; use a new project folder"
                        .into(),
                );
            }
            return Ok(Self { path, file });
        }

        let now = unix_time();
        let file = ProjectFile {
            format_version: FORMAT_VERSION,
            application_version: env!("CARGO_PKG_VERSION").to_owned(),
            pipeline,
            images: images_text,
            colmap: colmap_text,
            input_fingerprint,
            created_at: now,
            updated_at: now,
            stages: Vec::new(),
        };
        let mut project = Self { path, file };
        project.save()?;
        Ok(project)
    }

    pub fn stage_fingerprint(&self, command: &Command) -> String {
        let mut hash = StableHash::new();
        hash.add(self.file.input_fingerprint.as_bytes());
        hash.add(command.get_program().as_encoded_bytes());
        for argument in command.get_args() {
            hash.add(argument.as_encoded_bytes());
        }
        format!("{:016x}", hash.finish())
    }

    pub fn stage_complete(&self, id: &str, fingerprint: &str) -> Result<bool, String> {
        let Some(record) = self.file.stages.iter().find(|record| record.id == id) else {
            return Ok(false);
        };
        if record.fingerprint != fingerprint {
            return Err(format!(
                "Stage '{id}' settings changed since this project was created; use a new project folder"
            ));
        }
        Ok(record.state == StageState::Complete)
    }

    pub fn set_stage(
        &mut self,
        id: &str,
        fingerprint: &str,
        state: StageState,
    ) -> Result<(), String> {
        if let Some(record) = self.file.stages.iter_mut().find(|record| record.id == id) {
            if record.fingerprint != fingerprint {
                return Err(format!(
                    "Stage '{id}' settings changed since this project was created; use a new project folder"
                ));
            }
            record.state = state;
            record.updated_at = unix_time();
        } else {
            self.file.stages.push(StageRecord {
                id: id.to_owned(),
                fingerprint: fingerprint.to_owned(),
                state,
                updated_at: unix_time(),
            });
        }
        self.save()
    }

    fn save(&mut self) -> Result<(), String> {
        self.file.updated_at = unix_time();
        let encoded = serde_json::to_vec_pretty(&self.file)
            .map_err(|error| format!("Cannot encode project: {error}"))?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, encoded).map_err(|error| format!("Cannot write project: {error}"))?;
        replace_file(&temp, &self.path)
    }
}

pub fn is_project_workspace(workspace: &Path) -> bool {
    workspace.join(FILE_NAME).is_file()
}

fn absolute_text(path: &Path) -> Result<String, String> {
    path.canonicalize()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("Cannot resolve path {}: {error}", path.display()))
}

fn input_fingerprint(images: &Path, colmap: &Path, pipeline: PipelineId) -> Result<String, String> {
    let mut hash = StableHash::new();
    hash.add(format!("{pipeline:?}").as_bytes());
    add_metadata(&mut hash, colmap)?;

    let mut photos = Vec::new();
    for entry in fs::read_dir(images).map_err(|error| format!("Cannot read photos: {error}"))? {
        let entry = entry.map_err(|error| format!("Cannot inspect photo: {error}"))?;
        let path = entry.path();
        if path.is_file() && is_photo(&path) {
            photos.push(path);
        }
    }
    photos.sort_by_key(|path| wide(path.as_os_str()));
    if photos.is_empty() {
        return Err("The photo folder contains no supported images".into());
    }
    for photo in photos {
        hash.add(
            &wide(photo.file_name().unwrap_or_default())
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        add_metadata(&mut hash, &photo)?;
    }
    Ok(format!("{:016x}", hash.finish()))
}

fn add_metadata(hash: &mut StableHash, path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    hash.add(&metadata.len().to_le_bytes());
    let modified = metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    hash.add(&modified.as_secs().to_le_bytes());
    hash.add(&modified.subsec_nanos().to_le_bytes());
    Ok(())
}

fn is_photo(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        matches!(
            extension.to_string_lossy().to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp"
        )
    })
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().collect()
}

fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    let mut from_wide = wide(from.as_os_str());
    from_wide.push(0);
    let mut to_wide = wide(to.as_os_str());
    to_wide.push(0);
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    // SAFETY: both buffers are terminated and remain alive for the call.
    if unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags) } != 0 {
        Ok(())
    } else {
        Err(format!(
            "Cannot atomically replace project file: {}",
            std::io::Error::last_os_error()
        ))
    }
}

struct StableHash(u64);

impl StableHash {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn add(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x100000001b3);
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "colmap-project-{}-{}-{tag}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn project_roundtrip_and_stage_resume_are_strict() {
        let root = fixture("roundtrip");
        let images = root.join("images");
        let workspace = root.join("workspace");
        let colmap = root.join("colmap.exe");
        fs::create_dir(&images).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(images.join("one.jpg"), b"photo").unwrap();
        fs::write(&colmap, b"engine").unwrap();

        let mut project =
            Project::open_or_create(&workspace, &images, &colmap, PipelineId::RtxDense).unwrap();
        let mut command = Command::new(&colmap);
        command.args(["feature_extractor", "--gpu", "1"]);
        let fingerprint = project.stage_fingerprint(&command);
        assert!(!project.stage_complete("features", &fingerprint).unwrap());
        project
            .set_stage("features", &fingerprint, StageState::Complete)
            .unwrap();

        let reopened =
            Project::open_or_create(&workspace, &images, &colmap, PipelineId::RtxDense).unwrap();
        assert!(reopened.stage_complete("features", &fingerprint).unwrap());
        assert!(reopened.stage_complete("features", "changed").is_err());
        assert!(workspace.join(FILE_NAME).is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_inputs_or_pipeline_cannot_reuse_project() {
        let root = fixture("identity");
        let images = root.join("images");
        let workspace = root.join("workspace");
        let colmap = root.join("colmap.exe");
        fs::create_dir(&images).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(images.join("one.jpg"), b"photo").unwrap();
        fs::write(&colmap, b"engine").unwrap();
        Project::open_or_create(&workspace, &images, &colmap, PipelineId::QuickSparse).unwrap();
        assert!(
            Project::open_or_create(&workspace, &images, &colmap, PipelineId::RtxDense).is_err()
        );
        fs::write(images.join("one.jpg"), b"changed-photo").unwrap();
        assert!(
            Project::open_or_create(&workspace, &images, &colmap, PipelineId::QuickSparse).is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
