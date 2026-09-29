use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

pub const DENSE_MINIMUM_BYTES: u64 = 10_000_000_000;

#[derive(Clone, Debug)]
pub struct DensePreflight {
    pub image_count: usize,
    pub source_bytes: u64,
    pub estimated_required_bytes: u64,
    pub available_bytes: u64,
}

impl DensePreflight {
    pub fn has_enough_space(&self) -> bool {
        self.available_bytes >= self.estimated_required_bytes
    }
}

pub fn estimate_dense_bytes(source_bytes: u64) -> u64 {
    DENSE_MINIMUM_BYTES.max(source_bytes.saturating_mul(20))
}

pub fn inspect(images: &Path, workspace: &Path) -> Result<DensePreflight, String> {
    let mut image_count = 0usize;
    let mut source_bytes = 0u64;
    for entry in fs::read_dir(images).map_err(|e| format!("Cannot scan photos: {e}"))? {
        let entry = entry.map_err(|e| format!("Cannot read photo entry: {e}"))?;
        let path = entry.path();
        let is_image = path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp"
            )
        });
        if is_image {
            image_count += 1;
            source_bytes = source_bytes.saturating_add(
                entry
                    .metadata()
                    .map_err(|e| format!("Cannot inspect photo: {e}"))?
                    .len(),
            );
        }
    }
    let volume_path = existing_ancestor(workspace)
        .ok_or_else(|| "Project folder has no existing parent volume".to_string())?;
    Ok(DensePreflight {
        image_count,
        source_bytes,
        estimated_required_bytes: estimate_dense_bytes(source_bytes),
        available_bytes: available_bytes(&volume_path)?,
    })
}

fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| candidate.exists())
        .map(Path::to_path_buf)
}

pub fn free_space(path: &Path) -> Result<u64, String> {
    let volume_path =
        existing_ancestor(path).ok_or_else(|| "Path has no existing parent volume".to_string())?;
    available_bytes(&volume_path)
}

fn available_bytes(path: &Path) -> Result<u64, String> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(format!(
            "Cannot query free disk space for {}",
            path.display()
        ))
    } else {
        Ok(available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dense_estimate_has_a_conservative_floor() {
        assert_eq!(estimate_dense_bytes(1_000_000), DENSE_MINIMUM_BYTES);
    }
    #[test]
    fn dense_estimate_scales_for_large_sources() {
        assert_eq!(estimate_dense_bytes(1_000_000_000), 20_000_000_000);
    }
}
