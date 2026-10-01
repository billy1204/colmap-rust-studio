//! Bounded, standard-library-only COLMAP sparse point preview loader.
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Cloud {
    pub points: Vec<Point>,
    pub total_points: u64,
    pub normalization: Normalization,
}

#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub xyz: [f32; 3],
    pub rgb: [u8; 3],
}

#[derive(Clone, Copy, Debug)]
pub struct Normalization {
    factor: f64,
    lo: [f64; 3],
    hi: [f64; 3],
    extent: f64,
}

impl Normalization {
    pub fn apply(self, xyz: [f64; 3]) -> Result<[f32; 3], String> {
        if !xyz.iter().all(|value| value.is_finite()) {
            return Err("Non-finite coordinate".into());
        }
        Ok(std::array::from_fn(|axis| {
            if self.extent == 0. {
                0.
            } else {
                (2. * ((xyz[axis] * self.factor - self.lo[axis]) / self.extent
                    - (self.hi[axis] - self.lo[axis]) / self.extent * 0.5)) as f32
            }
        }))
    }
}

pub fn load(path: &Path) -> Result<Cloud, String> {
    use std::io::{BufReader, Seek, SeekFrom};
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Expected a regular points3D.bin file".into());
    }
    let file_len = metadata.len();
    let mut reader = BufReader::new(file);
    let total_points = u64::from_le_bytes(read_bytes(&mut reader)?);
    // Fixed fields occupy 51 bytes per point, before any tracks.
    if total_points == 0 {
        return Err("Point cloud has zero points".into());
    }
    if total_points > 10_000_000 || total_points > file_len.saturating_sub(8) / 51 {
        return Err("Invalid or unsupported point count".into());
    }
    let keep = total_points.min(100_000);
    let mut retained = Vec::with_capacity(keep as usize);
    for index in 0..total_points {
        let _id = read_bytes::<8>(&mut reader)?;
        let mut xyz = [0.; 3];
        for v in &mut xyz {
            *v = f64::from_le_bytes(read_bytes(&mut reader)?);
            if !v.is_finite() {
                return Err("Non-finite point coordinate".into());
            }
        }
        let rgb = read_bytes::<3>(&mut reader)?;
        let _error = read_bytes::<8>(&mut reader)?;
        let tracks = u64::from_le_bytes(read_bytes(&mut reader)?);
        let track_bytes = tracks.checked_mul(8).ok_or("Track length overflow")?;
        let end = reader
            .stream_position()
            .map_err(|e| e.to_string())?
            .checked_add(track_bytes)
            .ok_or("Track offset overflow")?;
        if end > file_len {
            return Err("Truncated track data".into());
        }
        reader
            .seek(SeekFrom::Start(end))
            .map_err(|e| e.to_string())?;
        let next = retained.len() as u64;
        if next < keep && (keep == 1 || index == next * (total_points - 1) / (keep - 1)) {
            retained.push((xyz, rgb));
        }
    }
    if reader.stream_position().map_err(|e| e.to_string())? != file_len {
        return Err("Unexpected trailing bytes in points3D.bin".into());
    }
    normalized_cloud(retained, total_points)
}

pub(crate) fn normalized_cloud(
    retained: Vec<([f64; 3], [u8; 3])>,
    total_points: u64,
) -> Result<Cloud, String> {
    if retained.is_empty() || total_points == 0 {
        return Err("Point cloud has zero points".into());
    }
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for (xyz, _) in &retained {
        for axis in 0..3 {
            lo[axis] = lo[axis].min(xyz[axis]);
            hi[axis] = hi[axis].max(xyz[axis]);
        }
    }
    // Halve only when a finite-coordinate subtraction would overflow.
    let factor = if (0..3).any(|a| !(hi[a] - lo[a]).is_finite()) {
        0.5
    } else {
        1.
    };
    for a in 0..3 {
        lo[a] *= factor;
        hi[a] *= factor;
    }
    let extent = (0..3).map(|a| hi[a] - lo[a]).fold(0., f64::max);
    let normalization = Normalization {
        factor,
        lo,
        hi,
        extent,
    };
    let points = retained
        .into_iter()
        .map(|(xyz, rgb)| {
            Ok(Point {
                // Divide before centering: avoids underflow for subnormal spans.
                xyz: normalization.apply(xyz)?,
                rgb,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Cloud {
        points,
        total_points,
        normalization,
    })
}

fn read_bytes<const N: usize>(reader: &mut impl std::io::Read) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| format!("Truncated or unreadable points3D.bin: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(bytes: &[u8]) -> Result<Cloud, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::var_os("HERMES_TEST_SCRATCH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join("test-scratch")
            });
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "colmap-points-{}-{}.bin",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).unwrap();
        let result = load(&path);
        std::fs::remove_file(path).unwrap();
        result
    }

    fn record(bytes: &mut Vec<u8>, id: u64, xyz: [f64; 3], rgb: [u8; 3], track: &[(u32, u32)]) {
        bytes.extend(id.to_le_bytes());
        for v in xyz {
            bytes.extend(v.to_le_bytes());
        }
        bytes.extend(rgb);
        bytes.extend(0.75_f64.to_le_bytes());
        bytes.extend((track.len() as u64).to_le_bytes());
        for (image, point) in track {
            bytes.extend(image.to_le_bytes());
            bytes.extend(point.to_le_bytes());
        }
    }

    fn pair() -> Vec<u8> {
        let mut bytes = 2_u64.to_le_bytes().to_vec();
        record(
            &mut bytes,
            42,
            [10., 20., 30.],
            [1, 2, 255],
            &[(7, 9), (88, 99)],
        );
        record(&mut bytes, 1000, [14., 22., 30.], [250, 0, 8], &[(3, 400)]);
        bytes
    }

    #[test]
    fn rejects_absurd_counts_before_reading_records() {
        for count in [u64::MAX, 10_000_001, 3] {
            let mut bytes = pair();
            bytes[..8].copy_from_slice(&count.to_le_bytes());
            let error = fixture(&bytes).unwrap_err();
            assert!(error.contains("count"), "{error}");
        }
    }

    #[test]
    fn rejects_absurd_track_lengths_without_panicking() {
        for tracks in [u64::MAX, u64::MAX / 8, 1000] {
            let mut bytes = 1_u64.to_le_bytes().to_vec();
            record(&mut bytes, 1, [1., 2., 3.], [0; 3], &[]);
            bytes[51..59].copy_from_slice(&tracks.to_le_bytes());
            assert!(fixture(&bytes).is_err());
        }
    }

    #[test]
    fn rejects_nonfinite_coordinates() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut bytes = pair();
            bytes[16..24].copy_from_slice(&value.to_le_bytes());
            assert!(fixture(&bytes).is_err());
        }
    }

    #[test]
    fn normalizes_degenerate_extreme_and_subnormal_clouds() {
        for (a, b, expected) in [
            ([5.; 3], [5.; 3], [0.; 3]),
            ([-f64::MAX; 3], [f64::MAX; 3], [-1.; 3]),
            ([0.; 3], [f64::from_bits(1); 3], [-1.; 3]),
        ] {
            let mut bytes = 2_u64.to_le_bytes().to_vec();
            record(&mut bytes, 1, a, [1; 3], &[]);
            record(&mut bytes, 2, b, [2; 3], &[]);
            let cloud = fixture(&bytes).unwrap();
            assert_eq!(cloud.points[0].xyz, expected);
            assert_eq!(cloud.points[1].xyz, expected.map(|v| -v));
        }
    }

    #[test]
    fn deterministic_sampling_uses_all_retained_bounds() {
        let count = 100_003_u64;
        let mut bytes = count.to_le_bytes().to_vec();
        for i in 0..count {
            record(
                &mut bytes,
                i,
                [i as f64, 0., 0.],
                [(i % 251) as u8, 7, 9],
                &[],
            );
        }
        let first = fixture(&bytes).unwrap();
        let second = fixture(&bytes).unwrap();
        assert_eq!(first.total_points, count);
        assert_eq!(first.points.len(), 100_000);
        assert_eq!(first.points[0].xyz, [-1., 0., 0.]);
        assert_eq!(first.points.last().unwrap().xyz, [1., 0., 0.]);
        for (i, (a, b)) in first.points.iter().zip(&second.points).enumerate() {
            let source = i as u64 * (count - 1) / 99_999;
            assert_eq!(a.rgb, [(source % 251) as u8, 7, 9]);
            assert_eq!(a.xyz, b.xyz);
            assert_eq!(a.rgb, b.rgb);
            assert!(
                (a.xyz[0] - (2. * source as f64 / (count - 1) as f64 - 1.) as f32).abs() < 1e-6
            );
        }
        // Index 33,334 is not selected by the documented sampling rule.
        let offset = 8 + 33_334 * 51 + 8;
        bytes[offset..offset + 8].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(
            fixture(&bytes).is_err(),
            "must validate discarded records too"
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = pair();
        bytes.push(0);
        assert!(fixture(&bytes).is_err());
    }

    #[test]
    fn rejects_zero_points() {
        assert!(fixture(&0_u64.to_le_bytes()).is_err());
    }

    #[test]
    fn rejects_every_truncated_prefix_including_final_track() {
        let bytes = pair();
        for end in 0..bytes.len() {
            assert!(fixture(&bytes[..end]).is_err(), "accepted prefix {end}");
        }
    }

    #[test]
    fn real_format_colors_and_uniform_centering() {
        let cloud = fixture(&pair()).unwrap();
        assert_eq!(cloud.total_points, 2);
        assert_eq!(cloud.points.len(), 2);
        assert_eq!(cloud.points[0].rgb, [1, 2, 255]);
        assert_eq!(cloud.points[1].rgb, [250, 0, 8]);
        assert_eq!(cloud.points[0].xyz, [-1., -0.5, 0.]);
        assert_eq!(cloud.points[1].xyz, [1., 0.5, 0.]);
    }
}
