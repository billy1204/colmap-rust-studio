/// Rotation matrix, rows × columns.
pub type Mat3 = [[f32; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// The preview camera. `rot` takes cloud coordinates into camera space
/// (x right, y up, depth away from the viewer); `target` is the point the
/// camera orbits around, always drawn at the view center.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub rot: Mat3,
    pub target: [f32; 3],
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            rot: IDENTITY,
            target: [0.0; 3],
            zoom: 3.0,
        }
    }
}

impl Camera {
    /// Perspective projection of a cloud point (centered, normalized
    /// coordinates) to `[screen x, screen y, depth]`; screen Y points down
    /// and the target lands on `[0, 0]`.
    pub fn project(&self, xyz: [f32; 3]) -> [f32; 3] {
        let t = self.target;
        project(
            apply(&self.rot, [xyz[0] - t[0], xyz[1] - t[1], xyz[2] - t[2]]),
            self.zoom,
        )
    }

    /// Cloud-space vector that moves a point at the target's depth by the
    /// screen `offset` (projected units, Y down): the inverse of `project`
    /// in the plane through the target that faces the camera.
    pub fn screen_to_world(&self, offset: [f32; 2]) -> [f32; 3] {
        let scale = pivot_scale(self.zoom);
        apply_transposed(&self.rot, [offset[0] / scale, -offset[1] / scale, 0.0])
    }

    /// Slide the view so the cloud follows a pointer move of `offset`
    /// (projected units, Y down).
    pub fn pan(&mut self, offset: [f32; 2]) {
        let w = self.screen_to_world(offset);
        self.target = std::array::from_fn(|i| self.target[i] - w[i]);
    }

    /// Turn the view by `angle` around `axis`, given in camera space.
    pub fn rotate(&mut self, axis: [f32; 3], angle: f32) {
        self.rot = orthonormalize(mul(&axis_angle(axis, angle), &self.rot));
    }

    /// Turntable orbit: `yaw` around the cloud's up axis, then `pitch`
    /// around the screen's horizontal axis.
    pub fn orbit(&mut self, yaw: f32, pitch: f32) {
        let up = column(&self.rot, 1);
        self.rotate(up, yaw);
        self.rotate([1.0, 0.0, 0.0], pitch);
    }

    /// Cloud axis `i` (0 = X, 1 = Y, 2 = Z) in camera space.
    pub fn axis(&self, i: usize) -> [f32; 3] {
        column(&self.rot, i)
    }
}

/// Distance from the eye to the target, in cloud units.
const EYE: f32 = 4.0;

/// Points nearer the eye than this are not drawn.
const NEAR: f32 = 0.25;

/// Screen units per cloud unit at the target's depth.
pub fn pivot_scale(zoom: f32) -> f32 {
    1.8 * zoom.clamp(0.1, 10.0) / EYE
}

/// Whether a projected point (from `Camera::project`) is far enough in
/// front of the eye to draw. Points at or behind the eye would otherwise
/// show up enlarged in the wrong place.
pub fn in_front(projected: [f32; 3]) -> bool {
    EYE + projected[2] >= NEAR
}

/// Perspective projection of a camera-space point.
fn project(c: [f32; 3], zoom: f32) -> [f32; 3] {
    let scale = 1.8 * zoom.clamp(0.1, 10.0) / (EYE + c[2]).max(0.1);
    [c[0] * scale, -c[1] * scale, c[2]]
}

pub fn apply(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

fn apply_transposed(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|c| m[0][c] * v[0] + m[1][c] * v[1] + m[2][c] * v[2])
}

fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    std::array::from_fn(|r| std::array::from_fn(|c| (0..3).map(|k| a[r][k] * b[k][c]).sum()))
}

fn column(m: &Mat3, i: usize) -> [f32; 3] {
    [m[0][i], m[1][i], m[2][i]]
}

pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len > 1e-9 { v.map(|x| x / len) } else { v }
}

/// Right-handed rotation by `angle` around `axis` (Rodrigues).
pub fn axis_angle(axis: [f32; 3], angle: f32) -> Mat3 {
    let [x, y, z] = normalize(axis);
    let (s, c) = angle.sin_cos();
    let t = 1.0 - c;
    [
        [t * x * x + c, t * x * y - s * z, t * x * z + s * y],
        [t * x * y + s * z, t * y * y + c, t * y * z - s * x],
        [t * x * z - s * y, t * y * z + s * x, t * z * z + c],
    ]
}

/// Remove the rounding drift that builds up over many small rotations.
fn orthonormalize(m: Mat3) -> Mat3 {
    let x = normalize(m[0]);
    let z = normalize(cross(x, m[1]));
    let y = cross(z, x);
    [x, y, z]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    fn screen(cam: &Camera, p: [f32; 3]) -> [f32; 2] {
        let s = cam.project(p);
        [s[0], s[1]]
    }

    fn turned(yaw: f32, pitch: f32, zoom: f32) -> Camera {
        let mut cam = Camera {
            zoom,
            ..Camera::default()
        };
        cam.orbit(yaw, pitch);
        cam
    }

    #[test]
    fn projection_basics() {
        let cam = Camera {
            zoom: 1.0,
            ..Camera::default()
        };
        assert_eq!(screen(&cam, [0.0; 3]), [0.0, 0.0]);
        let p = cam.project([1.0, 0.0, 0.0]);
        assert!(p[0] > 0.0 && p[1].abs() < 1e-6);
        let z = Camera { zoom: 2.0, ..cam }.project([1.0, 0.0, 0.0]);
        assert!((z[0] - 2.0 * p[0]).abs() < 1e-6);
        // Up is up on screen (screen Y points down).
        assert!(cam.project([0.0, 1.0, 0.0])[1] < 0.0);
        // A quarter turn around the up axis puts +X straight ahead.
        let r = turned(std::f32::consts::FRAC_PI_2, 0.0, 1.0).project([1.0, 0.0, 0.0]);
        assert!(r[0].abs() < 1e-6);
    }

    #[test]
    fn rotations_stay_rigid() {
        let mut cam = Camera::default();
        for i in 0..500 {
            cam.rotate([0.3, -1.0, 0.5], 0.01 * i as f32);
            cam.orbit(0.02, -0.01);
        }
        for i in 0..3 {
            let a = cam.axis(i);
            assert!((dot(a, a) - 1.0).abs() < 1e-4);
            assert!(dot(a, cam.axis((i + 1) % 3)).abs() < 1e-4);
        }
        // Right-handed: X × Y = Z.
        let z = cross(cam.axis(0), cam.axis(1));
        assert!(dot(z, cam.axis(2)) > 0.999);
    }

    #[test]
    fn axis_angle_turns_x_into_y() {
        let v = apply(
            &axis_angle([0.0, 0.0, 1.0], std::f32::consts::FRAC_PI_2),
            [1.0, 0.0, 0.0],
        );
        assert!((v[0]).abs() < 1e-6 && (v[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn screen_to_world_inverts_project_at_the_target() {
        for (yaw, pitch, zoom) in [(0.0, 0.0, 1.0), (0.7, -0.4, 2.5), (4.0, 1.5, 0.3)] {
            let cam = turned(yaw, pitch, zoom);
            let w = cam.screen_to_world([0.12, -0.05]);
            assert!(close(screen(&cam, w), [0.12, -0.05]));
            // It lies in the plane through the target, facing the camera.
            assert!(cam.project(w)[2].abs() < 1e-5);
        }
    }

    #[test]
    fn pan_moves_the_target_point_with_the_pointer() {
        let mut cam = turned(0.8, -0.5, 2.0);
        let point = cam.target;
        cam.pan([0.07, -0.03]);
        assert!(close(screen(&cam, point), [0.07, -0.03]));
    }

    #[test]
    fn points_at_or_behind_the_eye_are_not_drawn() {
        // Move the view center back so the eye sits inside the cloud, at z = -0.5.
        let cam = Camera {
            target: [0.0, 0.0, 3.5],
            ..Camera::default()
        };
        assert!(in_front(cam.project([0.0, 0.0, 1.0])));
        assert!(!in_front(cam.project([0.0, 0.0, -0.4])));
        assert!(!in_front(cam.project([0.3, 0.2, -1.0])));
        assert!(in_front(Camera::default().project([1.0, 1.0, -1.0])));
    }

    #[test]
    fn orbit_pivots_around_the_target() {
        let mut cam = turned(0.3, 0.2, 1.5);
        cam.target = cam.screen_to_world([0.2, 0.1]);
        let target = cam.target;
        for _ in 0..3 {
            cam.orbit(1.1, -0.6);
            cam.rotate([0.2, 0.9, -0.3], 0.8);
            assert!(close(screen(&cam, target), [0.0, 0.0]));
        }
    }
}
