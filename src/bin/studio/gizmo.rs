//! Move / Rotate / Scale gizmo for the 3D preview. It sits at the camera's
//! target in the middle of the view and moves the camera, never the cloud:
//! Move slides the view along an axis or plane, Rotate turns it around an
//! axis, and Scale zooms (a camera can't stretch along one axis).

use colmap_launcher::orbit::{Camera, apply, axis_angle, cross, normalize, pivot_scale};
use eframe::egui::{self, Color32, Painter, Pos2, Shape, Stroke, Vec2};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    Move,
    #[default]
    Rotate,
    Scale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    /// Move arrow or Scale cube on cloud axis `i`.
    Axis(usize),
    /// Move square for the plane normal to cloud axis `i`.
    Plane(usize),
    /// Move: free pan in the view plane. Scale: zoom.
    Center,
    /// Rotate ring around cloud axis `i`.
    Ring(usize),
    /// Outer ring: roll around the line of sight.
    ViewRing,
    /// Inside the ball: free orbit.
    Ball,
}

/// A handle being dragged. `grab` is the camera-space point on a ring the
/// pointer holds, so the ring turns to follow it.
#[derive(Clone, Copy, Debug)]
pub struct Drag {
    pub handle: Handle,
    mode: Mode,
    grab: [f32; 3],
}

const AXIS_COLORS: [Color32; 3] = [
    Color32::from_rgb(235, 80, 80),
    Color32::from_rgb(90, 200, 90),
    Color32::from_rgb(80, 120, 245),
];
const HOT: Color32 = Color32::from_rgb(255, 210, 60);
const RING_SAMPLES: usize = 72;

/// Gizmo size in pixels for a preview of `rect`.
pub fn radius(rect: egui::Rect) -> f32 {
    (rect.width().min(rect.height()) * 0.18).clamp(40.0, 80.0)
}

/// Screen direction (Y down) of a camera-space vector, ignoring perspective.
fn flat(v: [f32; 3]) -> Vec2 {
    Vec2::new(v[0], -v[1])
}

fn ring_points(axis: [f32; 3]) -> Vec<[f32; 3]> {
    let u = normalize(cross(
        axis,
        if axis[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        },
    ));
    let v = cross(axis, u);
    (0..RING_SAMPLES)
        .map(|k| {
            let (s, c) = (k as f32 * std::f32::consts::TAU / RING_SAMPLES as f32).sin_cos();
            std::array::from_fn(|i| c * u[i] + s * v[i])
        })
        .collect()
}

fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_sq().max(1e-6)).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

/// Corners of the Move square for the plane normal to axis `i`.
fn plane_quad(cam: &Camera, center: Pos2, r: f32, i: usize) -> [Pos2; 4] {
    let (a, b) = (flat(cam.axis((i + 1) % 3)), flat(cam.axis((i + 2) % 3)));
    [(0.25, 0.25), (0.45, 0.25), (0.45, 0.45), (0.25, 0.45)]
        .map(|(s, t)| center + (a * s + b * t) * r)
}

/// An axis pointing almost straight at the viewer can't be dragged along.
fn axis_usable(cam: &Camera, i: usize) -> bool {
    flat(cam.axis(i)).length() > 0.2
}

/// The handle under `pos`, if any, with where a ring was grabbed.
pub fn hit(mode: Mode, cam: &Camera, center: Pos2, r: f32, pos: Pos2) -> Option<Drag> {
    let drag = |handle| Drag {
        handle,
        mode,
        grab: [0.0; 3],
    };
    match mode {
        Mode::Move | Mode::Scale => {
            if pos.distance(center) < 10.0 {
                return Some(drag(Handle::Center));
            }
            let axis = (0..3).filter(|&i| axis_usable(cam, i)).find(|&i| {
                let dir = flat(cam.axis(i));
                segment_distance(pos, center + dir * r * 0.3, center + dir * (r + 14.0)) < 8.0
            });
            if let Some(i) = axis {
                return Some(drag(Handle::Axis(i)));
            }
            if mode == Mode::Move {
                for i in 0..3 {
                    let q = plane_quad(cam, center, r, i);
                    let mid = q[0] + (q[2] - q[0]) * 0.5;
                    if pos.distance(mid) < 9.0 {
                        return Some(drag(Handle::Plane(i)));
                    }
                }
            }
            None
        }
        Mode::Rotate => {
            // Nearest front-facing ring point within reach.
            let mut best: Option<(f32, usize, [f32; 3])> = None;
            for i in 0..3 {
                for p in ring_points(cam.axis(i)) {
                    let d = pos.distance(center + flat(p) * r);
                    if p[2] <= 0.05 && d < 8.0 && best.is_none_or(|(bd, ..)| d < bd) {
                        best = Some((d, i, p));
                    }
                }
            }
            if let Some((_, i, grab)) = best {
                return Some(Drag {
                    handle: Handle::Ring(i),
                    mode,
                    grab,
                });
            }
            let off = pos - center;
            if (off.length() - (r + 12.0)).abs() < 6.0 {
                let grab = normalize([off.x, -off.y, 0.0]);
                return Some(Drag {
                    handle: Handle::ViewRing,
                    mode,
                    grab,
                });
            }
            (off.length() < r).then(|| drag(Handle::Ball))
        }
    }
}

/// Apply a pointer move of `delta` pixels while dragging. `scale` is the
/// preview's pixels per projected unit; `r` the gizmo radius.
pub fn drag(drag: &mut Drag, cam: &mut Camera, delta: Vec2, scale: f32, r: f32) {
    // Pixels per cloud unit along a camera-space direction at the target.
    let k = scale * pivot_scale(cam.zoom);
    let zoom_by = |cam: &mut Camera, amount: f32| {
        cam.zoom = (cam.zoom * (amount * 0.01).exp()).clamp(0.1, 10.0);
    };
    match drag.handle {
        // The view slides so the cloud follows the pointer along the axis.
        Handle::Axis(i) if drag.mode == Mode::Move => {
            let along = flat(cam.axis(i)) * k;
            if along.length_sq() > 1e-6 {
                cam.target[i] -= delta.dot(along) / along.length_sq();
            }
        }
        Handle::Axis(i) => zoom_by(cam, delta.dot(flat(cam.axis(i)).normalized())),
        Handle::Plane(i) => {
            let (j, l) = ((i + 1) % 3, (i + 2) % 3);
            let (a, b) = (flat(cam.axis(j)) * k, flat(cam.axis(l)) * k);
            let det = a.x * b.y - a.y * b.x;
            if det.abs() > 1e-3 {
                cam.target[j] -= (delta.x * b.y - delta.y * b.x) / det;
                cam.target[l] -= (a.x * delta.y - a.y * delta.x) / det;
            }
        }
        Handle::Center if drag.mode == Mode::Move => cam.pan((delta / scale).into()),
        Handle::Center => zoom_by(cam, delta.x - delta.y),
        Handle::Ring(_) | Handle::ViewRing => {
            let axis = match drag.handle {
                Handle::Ring(i) => cam.axis(i),
                _ => [0.0, 0.0, 1.0],
            };
            // Turn by the angle that moves the grabbed point along with the pointer.
            let tangent = flat(cross(axis, drag.grab)) * r;
            if tangent.length_sq() > 1.0 {
                let angle = delta.dot(tangent) / tangent.length_sq();
                cam.rotate(axis, angle);
                drag.grab = apply(&axis_angle(axis, angle), drag.grab);
            }
        }
        Handle::Ball => cam.orbit(delta.x * 0.008, delta.y * 0.008),
    }
}

pub fn paint(
    painter: &Painter,
    mode: Mode,
    cam: &Camera,
    center: Pos2,
    r: f32,
    hot: Option<Handle>,
) {
    let color = |h: Handle, base: Color32| if hot == Some(h) { HOT } else { base };
    // Draw far axes first so near ones end up on top.
    let mut order = [0, 1, 2];
    order.sort_by(|&a, &b| cam.axis(b)[2].total_cmp(&cam.axis(a)[2]));
    match mode {
        Mode::Move | Mode::Scale => {
            if mode == Mode::Move {
                for i in order {
                    let q = plane_quad(cam, center, r, i);
                    let c = color(Handle::Plane(i), AXIS_COLORS[i]);
                    painter.add(Shape::convex_polygon(
                        q.to_vec(),
                        c.gamma_multiply(0.45),
                        Stroke::new(1.0, c),
                    ));
                }
            }
            for i in order {
                if !axis_usable(cam, i) {
                    continue;
                }
                let dir = flat(cam.axis(i));
                let c = color(Handle::Axis(i), AXIS_COLORS[i]);
                let tip = center + dir * r;
                painter.line_segment([center + dir * r * 0.12, tip], Stroke::new(2.5, c));
                if mode == Mode::Move {
                    let n = dir.normalized();
                    let side = n.rot90() * 6.0;
                    painter.add(Shape::convex_polygon(
                        vec![tip + n * 14.0, tip + side, tip - side],
                        c,
                        Stroke::NONE,
                    ));
                } else {
                    painter.rect_filled(
                        egui::Rect::from_center_size(tip, Vec2::splat(11.0)),
                        2.0,
                        c,
                    );
                }
            }
            painter.circle_filled(center, 5.5, color(Handle::Center, Color32::from_gray(225)));
        }
        Mode::Rotate => {
            let ball = if hot == Some(Handle::Ball) { 70 } else { 45 };
            painter.circle_filled(
                center,
                r,
                Color32::from_white_alpha(ball).gamma_multiply(0.35),
            );
            painter.circle_stroke(center, r, Stroke::new(1.0, Color32::from_white_alpha(120)));
            painter.circle_stroke(
                center,
                r + 12.0,
                Stroke::new(2.0, color(Handle::ViewRing, Color32::from_gray(210))),
            );
            for i in order {
                let c = color(Handle::Ring(i), AXIS_COLORS[i]);
                let pts = ring_points(cam.axis(i));
                for k in 0..pts.len() {
                    let (a, b) = (pts[k], pts[(k + 1) % pts.len()]);
                    // The far half of each ring is drawn faint, as in a glass ball.
                    let back = a[2] + b[2] > 0.0;
                    let stroke = if back {
                        Stroke::new(1.0, c.gamma_multiply(0.3))
                    } else {
                        Stroke::new(2.5, c)
                    };
                    painter.line_segment([center + flat(a) * r, center + flat(b) * r], stroke);
                }
            }
            painter.circle_filled(center, 2.5, Color32::from_gray(230));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: f32 = 70.0;
    const SCALE: f32 = 500.0;

    fn view() -> Camera {
        let mut cam = Camera::default();
        cam.orbit(0.6, 0.35);
        cam
    }

    /// Where a cloud point lands on screen, in pixels from the view center.
    fn on_screen(cam: &Camera, p: [f32; 3]) -> Vec2 {
        let s = cam.project(p);
        Vec2::new(s[0], s[1]) * SCALE
    }

    #[test]
    fn handles_are_found_where_they_are_drawn() {
        let cam = view();
        let c = Pos2::new(300.0, 200.0);
        let tip = c + flat(cam.axis(0)) * R;
        assert_eq!(
            hit(Mode::Move, &cam, c, R, c).unwrap().handle,
            Handle::Center
        );
        assert_eq!(
            hit(Mode::Move, &cam, c, R, tip).unwrap().handle,
            Handle::Axis(0)
        );
        assert_eq!(
            hit(Mode::Scale, &cam, c, R, tip).unwrap().handle,
            Handle::Axis(0)
        );
        let q = plane_quad(&cam, c, R, 1);
        assert_eq!(
            hit(Mode::Move, &cam, c, R, q[0] + (q[2] - q[0]) * 0.5)
                .unwrap()
                .handle,
            Handle::Plane(1)
        );
        assert_eq!(
            hit(Mode::Rotate, &cam, c, R, c + Vec2::new(0.0, -(R + 12.0)))
                .unwrap()
                .handle,
            Handle::ViewRing
        );
        assert!(hit(Mode::Move, &cam, c, R, c + Vec2::splat(200.0)).is_none());
        assert!(hit(Mode::Rotate, &cam, c, R, c + Vec2::splat(200.0)).is_none());
    }

    #[test]
    fn move_arrow_slides_the_cloud_with_the_pointer() {
        let mut cam = view();
        let point = cam.target;
        let mut d = Drag {
            handle: Handle::Axis(0),
            mode: Mode::Move,
            grab: [0.0; 3],
        };
        // Drag 30 px straight along the X arrow.
        let delta = flat(cam.axis(0)).normalized() * 30.0;
        drag(&mut d, &mut cam, delta, SCALE, R);
        assert!((on_screen(&cam, point) - delta).length() < 0.5);
        // Only X moved.
        assert_eq!(cam.target[1..], [0.0, 0.0]);
    }

    #[test]
    fn move_plane_and_center_follow_the_pointer() {
        for handle in [Handle::Plane(2), Handle::Center] {
            let mut cam = view();
            let point = cam.target;
            let mut d = Drag {
                handle,
                mode: Mode::Move,
                grab: [0.0; 3],
            };
            let delta = Vec2::new(12.0, -20.0);
            drag(&mut d, &mut cam, delta, SCALE, R);
            assert!(
                (on_screen(&cam, point) - delta).length() < 0.5,
                "{handle:?}"
            );
        }
    }

    #[test]
    fn ring_follows_the_pointer() {
        let mut cam = view();
        let c = Pos2::ZERO;
        // Grab the Y ring at its most front-facing point, then drag along it.
        let grab = ring_points(cam.axis(1))
            .into_iter()
            .min_by(|a, b| a[2].total_cmp(&b[2]))
            .unwrap();
        let mut d = hit(Mode::Rotate, &cam, c, R, c + flat(grab) * R).unwrap();
        assert_eq!(d.handle, Handle::Ring(1));
        let before = c + flat(d.grab) * R;
        let tangent = flat(cross(cam.axis(1), d.grab)).normalized();
        for _ in 0..10 {
            drag(&mut d, &mut cam, tangent * 2.0, SCALE, R);
        }
        let after = c + flat(d.grab) * R;
        assert!(((after - before).length() - 20.0).abs() < 2.0);
        // Turning around the Y axis keeps the Y axis where it was.
        assert!((cam.axis(1)[1] - view().axis(1)[1]).abs() < 1e-4);
    }

    #[test]
    fn scale_handles_zoom() {
        let mut cam = view();
        let zoom = cam.zoom;
        let mut d = Drag {
            handle: Handle::Axis(0),
            mode: Mode::Scale,
            grab: [0.0; 3],
        };
        // Outward along the X handle zooms in, without moving the view.
        let outward = flat(cam.axis(0)).normalized() * 40.0;
        drag(&mut d, &mut cam, outward, SCALE, R);
        assert!(cam.zoom > zoom);
        assert_eq!(cam.target, [0.0; 3]);
        // Left and down on the center zooms back out.
        let zoomed = cam.zoom;
        let mut d = Drag {
            handle: Handle::Center,
            mode: Mode::Scale,
            grab: [0.0; 3],
        };
        drag(&mut d, &mut cam, Vec2::new(-40.0, 40.0), SCALE, R);
        assert!(cam.zoom < zoomed);
    }
}
