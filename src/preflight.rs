use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

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

#[cfg(windows)]
fn available_bytes(path: &Path) -> Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
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

/// Bytes available to an unprivileged user on the filesystem holding `path`.
#[cfg(unix)]
fn available_bytes(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("Path contains a NUL byte: {}", path.display()))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a writable, correctly sized struct.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(format!(
            "Cannot query free disk space for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    #[allow(clippy::unnecessary_cast)] // field widths differ across platforms
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
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
    #[test]
    fn free_space_is_reported_for_temp_dir() {
        let free = free_space(&std::env::temp_dir().join("not-created-yet")).unwrap();
        assert!(free > 0);
    }
}
