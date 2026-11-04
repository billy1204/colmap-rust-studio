/// Perspective projection of centered, normalized points; screen Y points down.
pub fn project(xyz: [f32; 3], yaw: f32, pitch: f32, zoom: f32) -> [f32; 3] {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let x = cy * xyz[0] + sy * xyz[2];
    let z = -sy * xyz[0] + cy * xyz[2];
    let y = cp * xyz[1] - sp * z;
    let depth = sp * xyz[1] + cp * z;
    let scale = 1.8 * zoom.clamp(0.1, 10.0) / (4.0 + depth).max(0.1);
    [x * scale, -y * scale, depth]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orbit_changes_projection_and_zoom_preserves_center() {
        assert_eq!(project([0.0; 3], 0.0, 0.0, 1.0)[0], 0.0);
        let p = project([1.0, 0.0, 0.0], 0.0, 0.0, 1.0);
        assert!(p[0] > 0.0 && p[1].abs() < 1e-6);
        let z = project([1.0, 0.0, 0.0], 0.0, 0.0, 2.0);
        assert!((z[0] - 2.0 * p[0]).abs() < 1e-6);
        let r = project([1.0, 0.0, 0.0], std::f32::consts::FRAC_PI_2, 0.0, 1.0);
        assert!(r[0].abs() < 1e-6);
        assert!(project([0.0, 1.0, 0.0], 0.0, 0.0, 1.0)[1] < 0.0);
    }
}
