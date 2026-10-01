//! Bounded readers for COLMAP's documented sparse binary camera formats.
use crate::point_cloud::Normalization;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const CAMERA_PARAM_COUNTS: [usize; 18] = [3, 4, 4, 5, 8, 8, 12, 5, 4, 5, 12, 16, 4, 5, 3, 4, 6, 2];
const MAX_NAME_BYTES: usize = 1_048_576;

#[derive(Clone, Debug)]
pub struct Scene {
    pub cameras: Vec<CameraPose>,
    pub modern_rig_format: bool,
}

#[derive(Clone, Debug)]
pub struct CameraPose {
    pub image_id: u32,
    pub name: String,
    pub center: [f64; 3],
    pub cam_from_world_rotation: [f64; 4],
    pub aspect: f64,
}

#[derive(Clone, Debug)]
pub struct CameraOverlay {
    pub image_id: u32,
    pub name: String,
    pub center: [f32; 3],
    pub corners: [[f32; 3]; 4],
}

impl Scene {
    pub fn overlays(
        &self,
        normalization: Normalization,
        display_size: f32,
    ) -> Result<Vec<CameraOverlay>, String> {
        self.cameras
            .iter()
            .map(|camera| {
                let center = normalization.apply(camera.center)?;
                let inverse = quat_conjugate(camera.cam_from_world_rotation);
                let half_y = display_size as f64;
                let half_x = half_y * camera.aspect.clamp(0.25, 4.0);
                let forward = display_size as f64 * 2.0;
                let mut corners = [[0.; 3]; 4];
                for (index, local) in [
                    [-half_x, -half_y, forward],
                    [half_x, -half_y, forward],
                    [half_x, half_y, forward],
                    [-half_x, half_y, forward],
                ]
                .into_iter()
                .enumerate()
                {
                    let direction = quat_rotate(inverse, local);
                    corners[index] =
                        std::array::from_fn(|axis| center[axis] + direction[axis] as f32);
                }
                Ok(CameraOverlay {
                    image_id: camera.image_id,
                    name: camera.name.clone(),
                    center,
                    corners,
                })
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct Rigid {
    rotation: [f64; 4],
    translation: [f64; 3],
}

#[derive(Debug)]
struct Rig {
    sensors: HashMap<(i32, u32), Option<Rigid>>,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    rig_id: u32,
    rig_from_world: Rigid,
}

pub fn load(model: &Path) -> Result<Scene, String> {
    let cameras = parse_cameras(&model.join("cameras.bin"))?;
    let rigs_path = model.join("rigs.bin");
    let frames_path = model.join("frames.bin");
    if rigs_path.exists() != frames_path.exists() {
        return Err("Modern sparse models require both rigs.bin and frames.bin".into());
    }
    let (rigs, image_frames) = if rigs_path.is_file() {
        (parse_rigs(&rigs_path)?, parse_frames(&frames_path)?)
    } else {
        (HashMap::new(), HashMap::new())
    };
    let modern_rig_format = !rigs.is_empty() || !image_frames.is_empty();
    let camera_poses = parse_images(
        &model.join("images.bin"),
        &cameras,
        modern_rig_format.then_some(&rigs),
        modern_rig_format.then_some(&image_frames),
    )?;
    Ok(Scene {
        cameras: camera_poses,
        modern_rig_format,
    })
}

fn parse_cameras(path: &Path) -> Result<HashMap<u32, (u64, u64)>, String> {
    let mut reader = BinaryReader::open(path, "cameras.bin")?;
    let count = reader.count("camera count", 24)?;
    let mut cameras = HashMap::with_capacity(count.min(100_000));
    for _ in 0..count {
        let id = reader.u32()?;
        let model_id = reader.i32()?;
        let param_count = CAMERA_PARAM_COUNTS
            .get(usize::try_from(model_id).unwrap_or(usize::MAX))
            .copied()
            .ok_or_else(|| format!("Unsupported camera model id {model_id}"))?;
        let width = reader.u64()?;
        let height = reader.u64()?;
        if width == 0 || height == 0 {
            return Err(format!("Camera {id} has invalid dimensions"));
        }
        for _ in 0..param_count {
            if !reader.f64()?.is_finite() {
                return Err(format!("Camera {id} has non-finite parameters"));
            }
        }
        if cameras.insert(id, (width, height)).is_some() {
            return Err(format!("Duplicate camera id {id}"));
        }
    }
    reader.finish()?;
    Ok(cameras)
}

fn parse_rigs(path: &Path) -> Result<HashMap<u32, Rig>, String> {
    let mut reader = BinaryReader::open(path, "rigs.bin")?;
    let count = reader.count("rig count", 8)?;
    let mut rigs = HashMap::with_capacity(count.min(100_000));
    for _ in 0..count {
        let id = reader.u32()?;
        let sensor_count = reader.u32()? as usize;
        if sensor_count > reader.remaining() as usize / 8 {
            return Err("rigs.bin has an invalid sensor count".into());
        }
        let mut sensors = HashMap::with_capacity(sensor_count);
        if sensor_count > 0 {
            let reference = (reader.i32()?, reader.u32()?);
            sensors.insert(reference, Some(identity()));
        }
        for _ in 1..sensor_count {
            let sensor = (reader.i32()?, reader.u32()?);
            let pose = if reader.u8()? != 0 {
                Some(reader.rigid()?)
            } else {
                None
            };
            if sensors.insert(sensor, pose).is_some() {
                return Err(format!("Rig {id} contains a duplicate sensor"));
            }
        }
        if rigs.insert(id, Rig { sensors }).is_some() {
            return Err(format!("Duplicate rig id {id}"));
        }
    }
    reader.finish()?;
    Ok(rigs)
}

fn parse_frames(path: &Path) -> Result<HashMap<u64, Frame>, String> {
    let mut reader = BinaryReader::open(path, "frames.bin")?;
    let count = reader.count("frame count", 68)?;
    let mut images = HashMap::new();
    for _ in 0..count {
        let _frame_id = reader.u32()?;
        let frame = Frame {
            rig_id: reader.u32()?,
            rig_from_world: reader.rigid()?,
        };
        let data_count = reader.u32()? as usize;
        if data_count > reader.remaining() as usize / 16 {
            return Err("frames.bin has an invalid data count".into());
        }
        for _ in 0..data_count {
            let sensor_type = reader.i32()?;
            let _sensor_id = reader.u32()?;
            let data_id = reader.u64()?;
            if sensor_type == 0 && images.insert(data_id, frame).is_some() {
                return Err(format!("Image {data_id} belongs to multiple frames"));
            }
        }
    }
    reader.finish()?;
    Ok(images)
}

fn parse_images(
    path: &Path,
    cameras: &HashMap<u32, (u64, u64)>,
    rigs: Option<&HashMap<u32, Rig>>,
    image_frames: Option<&HashMap<u64, Frame>>,
) -> Result<Vec<CameraPose>, String> {
    let mut reader = BinaryReader::open(path, "images.bin")?;
    let count = reader.count("image count", 72)?;
    let mut output = Vec::with_capacity(count.min(1_000_000));
    for _ in 0..count {
        let image_id = reader.u32()?;
        let serialized_pose = reader.rigid()?;
        let camera_id = reader.u32()?;
        let name = reader.string()?;
        let point_count = reader.count("point2D count", 24)?;
        reader.skip(
            u64::try_from(point_count)
                .ok()
                .and_then(|value| value.checked_mul(24))
                .ok_or("Observation byte count overflow")?,
        )?;
        let &(width, height) = cameras
            .get(&camera_id)
            .ok_or_else(|| format!("Image {image_id} references missing camera {camera_id}"))?;
        let pose = if let (Some(rigs), Some(frames)) = (rigs, image_frames) {
            let frame = frames
                .get(&(image_id as u64))
                .ok_or_else(|| format!("No frame contains image {image_id}"))?;
            let rig = rigs.get(&frame.rig_id).ok_or_else(|| {
                format!("Image {image_id} references missing rig {}", frame.rig_id)
            })?;
            let sensor = rig
                .sensors
                .get(&(0, camera_id))
                .ok_or_else(|| format!("Rig {} does not contain camera {camera_id}", frame.rig_id))?
                .ok_or_else(|| {
                    format!("Rig {} has no pose for camera {camera_id}", frame.rig_id)
                })?;
            compose(sensor, frame.rig_from_world)
        } else {
            serialized_pose
        };
        output.push(CameraPose {
            image_id,
            name,
            center: projection_center(pose),
            cam_from_world_rotation: pose.rotation,
            aspect: width as f64 / height as f64,
        });
    }
    reader.finish()?;
    Ok(output)
}

fn identity() -> Rigid {
    Rigid {
        rotation: [1., 0., 0., 0.],
        translation: [0.; 3],
    }
}

fn compose(a: Rigid, b: Rigid) -> Rigid {
    let rotated = quat_rotate(a.rotation, b.translation);
    Rigid {
        rotation: quat_multiply(a.rotation, b.rotation),
        translation: std::array::from_fn(|axis| rotated[axis] + a.translation[axis]),
    }
}

fn projection_center(transform: Rigid) -> [f64; 3] {
    quat_rotate(
        quat_conjugate(transform.rotation),
        transform.translation.map(|value| -value),
    )
}

fn quat_conjugate(q: [f64; 4]) -> [f64; 4] {
    [q[0], -q[1], -q[2], -q[3]]
}

fn quat_multiply(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
        a[0] * b[1] + a[1] * b[0] + a[2] * b[3] - a[3] * b[2],
        a[0] * b[2] - a[1] * b[3] + a[2] * b[0] + a[3] * b[1],
        a[0] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[0],
    ]
}

fn quat_rotate(q: [f64; 4], v: [f64; 3]) -> [f64; 3] {
    let qv = [q[1], q[2], q[3]];
    let uv = cross(qv, v);
    let uuv = cross(qv, uv);
    std::array::from_fn(|axis| v[axis] + 2. * (q[0] * uv[axis] + uuv[axis]))
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

struct BinaryReader {
    inner: BufReader<File>,
    length: u64,
    offset: u64,
    label: &'static str,
}

impl BinaryReader {
    fn open(path: &Path, label: &'static str) -> Result<Self, String> {
        let file = File::open(path).map_err(|error| format!("Cannot open {label}: {error}"))?;
        let length = file
            .metadata()
            .map_err(|error| format!("Cannot inspect {label}: {error}"))?
            .len();
        Ok(Self {
            inner: BufReader::new(file),
            length,
            offset: 0,
            label,
        })
    }

    fn remaining(&self) -> u64 {
        self.length.saturating_sub(self.offset)
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], String> {
        if self.remaining() < N as u64 {
            return Err(format!(
                "{} is truncated at byte {}",
                self.label, self.offset
            ));
        }
        let mut bytes = [0; N];
        self.inner
            .read_exact(&mut bytes)
            .map_err(|error| format!("Cannot read {}: {error}", self.label))?;
        self.offset += N as u64;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes::<1>()?[0])
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.bytes()?))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.bytes()?))
    }

    fn rigid(&mut self) -> Result<Rigid, String> {
        let rotation = [self.f64()?, self.f64()?, self.f64()?, self.f64()?];
        let translation = [self.f64()?, self.f64()?, self.f64()?];
        if !rotation
            .iter()
            .chain(&translation)
            .all(|value| value.is_finite())
        {
            return Err(format!("{} contains a non-finite pose", self.label));
        }
        let norm = rotation.iter().map(|value| value * value).sum::<f64>();
        if !(0.999..=1.001).contains(&norm) {
            return Err(format!("{} contains an invalid quaternion", self.label));
        }
        Ok(Rigid {
            rotation,
            translation,
        })
    }

    fn string(&mut self) -> Result<String, String> {
        let mut bytes = Vec::new();
        loop {
            let byte = self.u8()?;
            if byte == 0 {
                break;
            }
            if bytes.len() >= MAX_NAME_BYTES {
                return Err(format!("{} contains an overlong image name", self.label));
            }
            bytes.push(byte);
        }
        String::from_utf8(bytes)
            .map_err(|_| format!("{} contains a non-UTF-8 image name", self.label))
    }

    fn count(&mut self, name: &str, minimum_bytes: u64) -> Result<usize, String> {
        let value = self.u64()?;
        if value > self.remaining() / minimum_bytes.max(1) || value > usize::MAX as u64 {
            return Err(format!("{} has an invalid {name}", self.label));
        }
        Ok(value as usize)
    }

    fn skip(&mut self, bytes: u64) -> Result<(), String> {
        if bytes > self.remaining() || bytes > i64::MAX as u64 {
            return Err(format!(
                "{} is truncated at byte {}",
                self.label, self.offset
            ));
        }
        self.inner
            .seek(SeekFrom::Current(bytes as i64))
            .map_err(|error| format!("Cannot seek {}: {error}", self.label))?;
        self.offset += bytes;
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.offset == self.length {
            Ok(())
        } else {
            Err(format!("{} contains trailing data", self.label))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend(value.to_le_bytes());
    }
    fn i32(bytes: &mut Vec<u8>, value: i32) {
        bytes.extend(value.to_le_bytes());
    }
    fn u64(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend(value.to_le_bytes());
    }
    fn f64(bytes: &mut Vec<u8>, value: f64) {
        bytes.extend(value.to_le_bytes());
    }
    fn rigid(bytes: &mut Vec<u8>, translation: [f64; 3]) {
        for value in [
            1.,
            0.,
            0.,
            0.,
            translation[0],
            translation[1],
            translation[2],
        ] {
            f64(bytes, value);
        }
    }
    fn fixture(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("sparse-scene-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        path
    }
    fn base_model(path: &Path, camera_id: u32) {
        let mut cameras = Vec::new();
        u64(&mut cameras, 1);
        u32(&mut cameras, camera_id);
        i32(&mut cameras, 0);
        u64(&mut cameras, 640);
        u64(&mut cameras, 480);
        for value in [500., 320., 240.] {
            f64(&mut cameras, value);
        }
        std::fs::write(path.join("cameras.bin"), cameras).unwrap();
        let mut images = Vec::new();
        u64(&mut images, 1);
        u32(&mut images, 2);
        rigid(&mut images, [4., 5., 6.]);
        u32(&mut images, camera_id);
        images.extend(b"images/frame.jpg\0");
        u64(&mut images, 0);
        std::fs::write(path.join("images.bin"), images).unwrap();
    }

    #[test]
    fn parses_legacy_camera_pose_and_aspect() {
        let path = fixture("legacy");
        base_model(&path, 1);
        let scene = load(&path).unwrap();
        assert!(!scene.modern_rig_format);
        assert_eq!(scene.cameras.len(), 1);
        assert_eq!(scene.cameras[0].center, [-4., -5., -6.]);
        assert!((scene.cameras[0].aspect - 4. / 3.).abs() < 1e-12);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn composes_modern_rig_and_frame_poses() {
        let path = fixture("rig");
        base_model(&path, 2);
        let mut rigs = Vec::new();
        u64(&mut rigs, 1);
        u32(&mut rigs, 7);
        u32(&mut rigs, 2);
        i32(&mut rigs, 0);
        u32(&mut rigs, 1);
        i32(&mut rigs, 0);
        u32(&mut rigs, 2);
        rigs.push(1);
        rigid(&mut rigs, [1., 0., 0.]);
        std::fs::write(path.join("rigs.bin"), rigs).unwrap();
        let mut frames = Vec::new();
        u64(&mut frames, 1);
        u32(&mut frames, 9);
        u32(&mut frames, 7);
        rigid(&mut frames, [0., 2., 0.]);
        u32(&mut frames, 1);
        i32(&mut frames, 0);
        u32(&mut frames, 2);
        u64(&mut frames, 2);
        std::fs::write(path.join("frames.bin"), frames).unwrap();
        let scene = load(&path).unwrap();
        assert!(scene.modern_rig_format);
        assert_eq!(scene.cameras[0].center, [-1., -2., 0.]);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn rejects_truncation_in_each_required_file() {
        for name in ["cameras.bin", "images.bin"] {
            let path = fixture(name);
            base_model(&path, 1);
            let bytes = std::fs::read(path.join(name)).unwrap();
            std::fs::write(path.join(name), &bytes[..bytes.len() - 1]).unwrap();
            assert!(load(&path).is_err(), "accepted truncated {name}");
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}
