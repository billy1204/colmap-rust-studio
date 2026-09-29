use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

const SETTINGS_VERSION: u32 = 1;
const MAX_RECENT: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProject {
    pub workspace: PathBuf,
    pub images: PathBuf,
    pub pipeline: String,
    pub last_opened: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub install: PathBuf,
    pub recent: Vec<RecentProject>,
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    version: u32,
    install: PathBuf,
    recent: Vec<RecentProject>,
}

impl Settings {
    pub fn record(&mut self, item: RecentProject) {
        self.recent
            .retain(|existing| existing.workspace != item.workspace);
        self.recent.insert(0, item);
        self.recent.truncate(MAX_RECENT);
    }
}

pub fn default_path() -> PathBuf {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    root.join("COLMAP Rust Studio").join("settings.json")
}

pub fn save_atomic(path: &Path, settings: &Settings) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Settings path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("Cannot create settings folder: {e}"))?;
    let temp = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec_pretty(&SettingsFile {
        version: SETTINGS_VERSION,
        install: settings.install.clone(),
        recent: settings.recent.clone(),
    })
    .map_err(|e| format!("Cannot encode settings: {e}"))?;
    let mut file = fs::File::create(&temp).map_err(|e| format!("Cannot create settings: {e}"))?;
    file.write_all(&encoded)
        .map_err(|e| format!("Cannot write settings: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("Cannot flush settings: {e}"))?;
    drop(file);
    replace_file(&temp, path)
}

pub fn load(path: &Path) -> Result<Settings, String> {
    if !path.exists() {
        return Ok(Settings::default());
    }
    let bytes = fs::read(path).map_err(|e| format!("Cannot read settings: {e}"))?;
    let file: SettingsFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("Settings are invalid: {e}"))?;
    if file.version != SETTINGS_VERSION {
        return Err(format!("Unsupported settings version {}", file.version));
    }
    Ok(Settings {
        install: file.install,
        recent: file.recent.into_iter().take(MAX_RECENT).collect(),
    })
}

fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    let wide = |path: &Path| {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        value.push(0);
        value
    };
    let from_wide = wide(from);
    let to_wide = wide(to);
    let ok = unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err("Cannot atomically replace settings file".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recent_projects_dedupe_move_to_front_and_cap() {
        let mut s = Settings::default();
        for i in 0..10 {
            s.record(RecentProject {
                workspace: PathBuf::from(format!("p{i}")),
                images: PathBuf::from("i"),
                pipeline: "quick".into(),
                last_opened: i,
            });
        }
        assert_eq!(s.recent.len(), 8);
        let newest = s.recent[3].clone();
        s.record(newest.clone());
        assert_eq!(s.recent[0].workspace, newest.workspace);
        assert_eq!(s.recent.len(), 8);
    }
    #[test]
    fn atomic_json_roundtrip() {
        let path =
            std::env::temp_dir().join(format!("colmap-settings-{}.json", std::process::id()));
        let mut s = Settings {
            install: PathBuf::from("engine"),
            recent: Vec::new(),
        };
        s.record(RecentProject {
            workspace: PathBuf::from("project"),
            images: PathBuf::from("photos"),
            pipeline: "dense".into(),
            last_opened: 7,
        });
        save_atomic(&path, &s).unwrap();
        let read = load(&path).unwrap();
        assert_eq!(read.install, s.install);
        assert_eq!(read.recent, s.recent);
        let _ = fs::remove_file(path);
    }
}
