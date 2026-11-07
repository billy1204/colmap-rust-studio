use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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
    /// Output parent folder the user last chose.
    pub output_parent: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    version: u32,
    install: PathBuf,
    recent: Vec<RecentProject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_parent: Option<PathBuf>,
}

impl Settings {
    pub fn record(&mut self, item: RecentProject) {
        self.recent
            .retain(|existing| existing.workspace != item.workspace);
        self.recent.insert(0, item);
        self.recent.truncate(MAX_RECENT);
    }
}

/// Settings location: `%LOCALAPPDATA%\COLMAP Rust Studio` on Windows,
/// `$XDG_CONFIG_HOME/colmap-rust-studio` (default `~/.config/...`) elsewhere.
pub fn default_path() -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        return root.join("COLMAP Rust Studio").join("settings.json");
    }
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            let home = crate::platform::home_dir();
            (!home.as_os_str().is_empty()).then(|| home.join(".config"))
        })
        .unwrap_or_else(std::env::temp_dir);
    root.join("colmap-rust-studio").join("settings.json")
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
        output_parent: settings.output_parent.clone(),
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
        output_parent: file.output_parent,
    })
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
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

/// `rename(2)` atomically replaces the destination on POSIX filesystems;
/// syncing the parent directory makes the new entry durable.
#[cfg(unix)]
fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).map_err(|e| format!("Cannot atomically replace settings file: {e}"))?;
    if let Some(parent) = to.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
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
            output_parent: Some(PathBuf::from("projects")),
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
        assert_eq!(read.output_parent, s.output_parent);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn settings_without_an_output_folder_still_load() {
        let path =
            std::env::temp_dir().join(format!("colmap-settings-old-{}.json", std::process::id()));
        fs::write(&path, r#"{"version":1,"install":"engine","recent":[]}"#).unwrap();
        let read = load(&path).unwrap();
        assert_eq!(read.install, PathBuf::from("engine"));
        assert_eq!(read.output_parent, None);
        let _ = fs::remove_file(path);
    }
}
