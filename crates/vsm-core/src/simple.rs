//! Experimental, explicitly constrained Local-camera calibration.
//! Inputs are standard camera/HMD poses only. Avatar probes are never consulted.
use crate::math::*;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

fn yaw(q: Quat) -> f64 {
    let f = rotate(q, [0., 0., -1.]);
    (-f[0]).atan2(-f[2])
}
fn yaw_q(y: f64) -> Quat {
    [0., (y / 2.).sin(), 0., (y / 2.).cos()]
}
fn angle(a: f64) -> f64 {
    (a + PI).rem_euclid(2. * PI) - PI
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalibrationSample {
    pub camera: Pose,
    pub hmd: Pose,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalCalibration {
    pub schema_version: u32,
    pub calibrated_at_ns: i64,
    pub camera_yaw: f64,
    /// Camera-to-pivot, expressed in the level camera's yaw frame.
    pub pivot_from_camera: Vec3,
    pub hmd: Pose,
    pub world_from_tracking_yaw: f64,
    pub mouth_forward_m: f64,
    pub arc_degrees: f64,
    pub residual_rms_m: f64,
    pub camera_radius_m: f64,
    pub head_motion_m: f64,
}

impl LocalCalibration {
    /// Operator conditions: Local fixed, untouched level lens, no stick translation,
    /// physically still head, mouth centered vertically, initially facing the lens.
    /// Circle fit certifies geometry only; it cannot establish these conditions.
    pub fn fit(samples: &[CalibrationSample], mouth_forward_m: f64) -> Result<Self> {
        ensure!(
            samples.len() >= 20 && samples.len() <= 10000,
            "校正用の旋回データが不足しています"
        );
        ensure!(
            mouth_forward_m.is_finite() && (0.0..=0.2).contains(&mouth_forward_m),
            "Invalid estimated mouth offset"
        );
        let mut normal = [[0.; 5]; 4];
        let mut angles = Vec::new();
        let mut previous = -1;
        let mut head_motion: f64 = 0.;
        let mut vertical: f64 = 0.;
        for s in samples {
            ensure!(
                s.camera.time_ns > previous,
                "Camera samples must be distinct and ordered"
            );
            previous = s.camera.time_ns;
            ensure!(
                s.camera
                    .position
                    .iter()
                    .chain(s.hmd.position.iter())
                    .all(|v| v.is_finite()),
                "Nonfinite position"
            );
            ensure!(
                s.camera.time_ns >= s.hmd.time_ns
                    && s.camera.time_ns - s.hmd.time_ns <= 150_000_000,
                "HMD observation is stale"
            );
            let q = unit(s.camera.rotation)?;
            unit(s.hmd.rotation)?;
            ensure!(
                dot(rotate(q, [0., 1., 0.]), [0., 1., 0.]) > (5f64.to_radians()).cos(),
                "カメラの水平を合わせてください"
            );
            let y = yaw(q);
            let unwrapped = angles.last().map_or(y, |last| last + angle(y - last));
            angles.push(unwrapped);
            head_motion = head_motion.max(norm(sub(s.hmd.position, samples[0].hmd.position)));
            vertical = vertical.max((s.camera.position[1] - samples[0].camera.position[1]).abs());
            let (sn, cs) = y.sin_cos();
            for (row, value) in [
                ([1., 0., cs, sn], s.camera.position[0]),
                ([0., 1., -sn, cs], s.camera.position[2]),
            ] {
                for i in 0..4 {
                    for j in 0..4 {
                        normal[i][j] += row[i] * row[j];
                    }
                    normal[i][4] += row[i] * value;
                }
            }
        }
        ensure!(head_motion <= 0.03, "校正中に頭の位置が動きました");
        ensure!(vertical <= 0.02, "校正中にカメラの高さが変わりました");
        let arc = (angles.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - angles.iter().copied().fold(f64::INFINITY, f64::min))
        .to_degrees();
        ensure!(arc >= 90., "旋回量が不足しています");
        // Pivoted elimination of the small least-squares system.
        for i in 0..4 {
            let pivot = (i..4)
                .max_by(|&a, &b| normal[a][i].abs().total_cmp(&normal[b][i].abs()))
                .unwrap();
            normal.swap(i, pivot);
            ensure!(normal[i][i].abs() > 1e-8, "旋回の基準を分離できません");
            let divisor = normal[i][i];
            for j in i..5 {
                normal[i][j] /= divisor;
            }
            for k in 0..4 {
                if k != i {
                    let factor = normal[k][i];
                    for j in i..5 {
                        normal[k][j] -= factor * normal[i][j];
                    }
                }
            }
        }
        let fit = normal.map(|r| r[4]);
        let radius = fit[2].hypot(fit[3]);
        ensure!(
            (0.05..=5.).contains(&radius),
            "校正用のカメラ距離が不適切です"
        );
        let error = samples
            .iter()
            .map(|s| {
                let r = rotate(yaw_q(yaw(s.camera.rotation)), [fit[2], 0., fit[3]]);
                (s.camera.position[0] - fit[0] - r[0]).powi(2)
                    + (s.camera.position[2] - fit[1] - r[2]).powi(2)
            })
            .sum::<f64>();
        let residual = (error / samples.len() as f64).sqrt();
        ensure!(
            residual <= 0.02,
            "旋回中の並進またはレンズ操作を分離できません"
        );
        let first = &samples[0];
        Ok(Self {
            schema_version: 1,
            calibrated_at_ns: samples.last().unwrap().camera.time_ns,
            camera_yaw: yaw(first.camera.rotation),
            pivot_from_camera: [-fit[2], 0., -fit[3]],
            hmd: first.hmd.clone(),
            world_from_tracking_yaw: angle(
                yaw(first.camera.rotation) + PI - yaw(first.hmd.rotation),
            ),
            mouth_forward_m,
            arc_degrees: arc,
            residual_rms_m: residual,
            camera_radius_m: radius,
            head_motion_m: head_motion,
        })
    }

    /// No tracking-scale assumption: physical head translation beyond 3 cm is
    /// unsupported in this first preview. Stick motion may change the world frame.
    pub fn estimate(&self, camera: &Pose, hmd: &Pose, at: i64) -> Result<Vec3> {
        ensure!(
            self.schema_version == 1 && at >= self.calibrated_at_ns,
            "校正前の区間です"
        );
        ensure!(
            at >= camera.time_ns && at >= hmd.time_ns && at - hmd.time_ns <= 150_000_000,
            "HMDデータ待ち"
        );
        let cq = unit(camera.rotation)?;
        let hq = unit(hmd.rotation)?;
        ensure!(
            camera
                .position
                .iter()
                .chain(hmd.position.iter())
                .all(|v| v.is_finite()),
            "Nonfinite pose"
        );
        ensure!(
            dot(rotate(cq, [0., 1., 0.]), [0., 1., 0.]) > (5f64.to_radians()).cos(),
            "カメラの水平条件が変わりました"
        );
        ensure!(
            norm(sub(hmd.position, self.hmd.position)) <= 0.03,
            "頭の実移動はこの試作の対応範囲外です"
        );
        let y = yaw(cq);
        let world = product(
            yaw_q(y - self.camera_yaw + self.world_from_tracking_yaw),
            hq,
        );
        let first = product(yaw_q(self.world_from_tracking_yaw), self.hmd.rotation);
        let offset = rotate(world, [0., 0., -self.mouth_forward_m]);
        let initial_offset = rotate(first, [0., 0., -self.mouth_forward_m]);
        let mut mouth = add(
            add(camera.position, rotate(yaw_q(y), self.pivot_from_camera)),
            offset,
        );
        // Initial lens height was explicitly aligned to the mouth, not the head.
        mouth[1] -= initial_offset[1];
        Ok(mouth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn samples() -> Vec<CalibrationSample> {
        (0..101)
            .map(|i| {
                let q = yaw_q(i as f64 / 100. * 2. * PI);
                let t = 1_000_000_000 + i * 20_000_000;
                CalibrationSample {
                    camera: Pose {
                        time_ns: t,
                        position: add([2., 1.3, -3.], rotate(q, [0., 0., 0.5])),
                        rotation: q,
                    },
                    hmd: Pose {
                        time_ns: t,
                        position: [0., 1.6, 0.],
                        rotation: yaw_q(PI),
                    },
                }
            })
            .collect()
    }
    #[test]
    fn orbit_and_held_out_translation() {
        let s = samples();
        let c = LocalCalibration::fit(&s, 0.07).unwrap();
        assert!(c.residual_rms_m < 1e-10);
        assert!((c.camera_radius_m - 0.5).abs() < 1e-10);
        let mut cam = s.last().unwrap().camera.clone();
        let mut h = s.last().unwrap().hmd.clone();
        cam.position = add(cam.position, [4., 0., 2.]);
        cam.time_ns += 1;
        h.time_ns += 1;
        let m = c.estimate(&cam, &h, cam.time_ns).unwrap();
        assert!(norm(sub(m, [6., 1.3, -0.93])) < 1e-10);
        h.position[0] += 0.2;
        assert!(c.estimate(&cam, &h, cam.time_ns).is_err());
    }
    #[test]
    fn rejects_unobservable_or_mixed_calibration() {
        let mut s = samples();
        assert!(LocalCalibration::fit(&s[..20], 0.07).is_err());
        s[40].camera.position[0] += 1.;
        assert!(LocalCalibration::fit(&s, 0.07).is_err());
        let mut s = samples();
        s[30].hmd.position[0] += 0.1;
        assert!(LocalCalibration::fit(&s, 0.07).is_err());
        let mut s = samples();
        s[20].camera.time_ns = s[19].camera.time_ns;
        assert!(LocalCalibration::fit(&s, 0.07).is_err());
    }
}
