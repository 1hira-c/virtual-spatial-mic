use crate::math::*;
use crate::osc::Message;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::f64::consts::PI;

#[derive(Clone, Serialize, Deserialize)]
struct Field {
    value: f64,
    time_ns: i64,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Sign {
    contact: f64,
    near_at_ns: i64,
    mismatch_at_ns: i64,
    inferred: bool,
}
impl Default for Sign {
    fn default() -> Self {
        Self {
            contact: -1.,
            near_at_ns: -10_000_000_000,
            mismatch_at_ns: -1,
            inferred: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LivePose {
    schema_version: u32,
    fields: BTreeMap<String, Field>,
    signs: [Sign; 9],
    camera: Pose,
    hmd: Pose,
    camera_seen: bool,
    hmd_seen: bool,
    mouth_signal_ns: i64,
    last_signal_ns: i64,
    reset_at_ns: i64,
    last_eval_ns: i64,
    last_yaw: f64,
    yaw_at_ns: i64,
    epoch: u64,
    source_mode: String,
    mouth_offset_m: Vec3,
    #[serde(default)]
    scale_changed_at_ns: Option<i64>,
    #[serde(default)]
    scale_invalid: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoseResult {
    pub valid: bool,
    pub relative_m: Vec3,
    pub mouth_m: Vec3,
    pub camera: Pose,
    pub reason: String,
    pub orientation: String,
    pub source_model: String,
    pub details: Value,
}
impl LivePose {
    pub fn new(offset: Vec3, mode: &str) -> Result<Self> {
        ensure!(
            offset.iter().all(|v| v.is_finite() && v.abs() <= 1.),
            "Invalid mouth offset"
        );
        ensure!(
            ["auto", "head_offset", "mouth_probe"].contains(&mode),
            "Invalid source mode"
        );
        Ok(Self {
            schema_version: 1,
            fields: BTreeMap::new(),
            signs: [Sign::default(); 9],
            camera: Pose::default(),
            hmd: Pose::default(),
            camera_seen: false,
            hmd_seen: false,
            mouth_signal_ns: -1,
            last_signal_ns: -1,
            reset_at_ns: -1,
            last_eval_ns: -1,
            last_yaw: 0.,
            yaw_at_ns: -10_000_000_000,
            epoch: 0,
            source_mode: mode.into(),
            mouth_offset_m: offset,
            scale_changed_at_ns: None,
            scale_invalid: false,
        })
    }
    pub fn checkpoint(&self) -> Value {
        serde_json::to_value(self).expect("Validated finite pose state")
    }
    pub fn restore(&mut self, value: &Value) -> Result<()> {
        let mut restored: Self = serde_json::from_value(value.clone())?;
        ensure!(
            restored.schema_version == 1 && restored.fields.len() <= 256,
            "Unsupported pose checkpoint"
        );
        ensure!(
            restored.fields.values().all(|f| f.value.is_finite())
                && restored.signs.iter().all(|s| s.contact.is_finite())
                && restored.last_yaw.is_finite(),
            "Nonfinite pose checkpoint"
        );
        for p in [&mut restored.camera, &mut restored.hmd] {
            ensure!(
                p.position.iter().all(|v| v.is_finite()),
                "Nonfinite checkpoint position"
            );
            p.rotation = unit(p.rotation)?;
        }
        restored.source_mode = self.source_mode.clone();
        restored.mouth_offset_m = self.mouth_offset_m;
        *self = restored;
        Ok(())
    }
    pub fn feed(
        &mut self,
        messages: &[Message],
        at: i64,
        snapshot: bool,
        request_start: i64,
    ) -> Result<()> {
        for m in messages {
            let a = &m.address;
            if a == "/avatar/change" && !snapshot {
                self.fields.clear();
                self.signs = [Sign::default(); 9];
                self.camera_seen = false;
                self.hmd_seen = false;
                self.reset_at_ns = at;
                self.epoch += 1;
                self.last_signal_ns = -1;
                self.mouth_signal_ns = -1;
                self.yaw_at_ns = -10_000_000_000;
                self.scale_changed_at_ns = None;
                self.scale_invalid = false;
                continue;
            }
            if snapshot
                && (self.reset_at_ns >= request_start
                    || self
                        .fields
                        .get(a)
                        .is_some_and(|f| f.time_ns >= request_start))
            {
                continue;
            }
            if !snapshot
                && m.typetag == ",ffffff"
                && (a == "/usercamera/Pose" || a == "/tracking/vrsystem/head/pose")
            {
                let values: Option<Vec<f64>> = m.values.iter().map(Value::as_f64).collect();
                let p = unity_pose(
                    &values.ok_or_else(|| anyhow::anyhow!("Invalid pose number"))?,
                    at,
                )?;
                if a == "/usercamera/Pose" {
                    self.camera = p;
                    self.camera_seen = true;
                } else {
                    self.hmd = p;
                    self.hmd_seen = true;
                    self.last_signal_ns = at;
                }
                continue;
            }
            if a == "/avatar/parameters/ScaleFactor" && m.typetag == ",f" && m.values.len() == 1 {
                let Some(v) = m.values[0]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v > 0. && (*v * 1000.).is_finite())
                else {
                    self.scale_invalid = true;
                    continue;
                };
                let changed = self
                    .fields
                    .get(a)
                    .is_some_and(|f| (f.value - v).abs() > 1e-6 * f.value.abs().max(v.abs()));
                if changed
                    || self.scale_invalid
                    || (!self.fields.contains_key(a) && (v - 1.).abs() > 1e-6)
                {
                    self.scale_changed_at_ns = Some(at);
                    self.signs = [Sign::default(); 9];
                }
                self.scale_invalid = false;
                self.fields.insert(
                    a.clone(),
                    Field {
                        value: v,
                        time_ns: at,
                    },
                );
                continue;
            }
            if a == "/avatar/parameters/VBS/Ref/ProbeVersion"
                && m.typetag == ",i"
                && m.values.len() == 1
                && m.values[0].is_i64()
            {
                self.fields.insert(
                    a.clone(),
                    Field {
                        value: m.values[0].as_f64().unwrap(),
                        time_ns: at,
                    },
                );
                continue;
            }
            let head = a.starts_with("/avatar/parameters/VBS/Ref/head/");
            let mouth = a.starts_with("/avatar/parameters/VBS/Ref/mouth/");
            if (head || mouth)
                && m.typetag == ",f"
                && m.values.len() == 1
                && let Some(v) = m.values[0].as_f64()
            {
                if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                    self.fields.remove(a);
                    continue;
                }
                self.fields.insert(
                    a.clone(),
                    Field {
                        value: v,
                        time_ns: at,
                    },
                );
                if head {
                    self.last_signal_ns = at;
                } else {
                    self.mouth_signal_ns = at;
                }
            }
        }
        Ok(())
    }
    fn vector(
        &mut self,
        probe: &str,
        kind: &str,
        base: usize,
        at: i64,
        inferred: &mut Vec<String>,
    ) -> std::result::Result<Vec3, String> {
        let direction = kind == "r";
        let label = if probe == "mouth" { "口元" } else { "頭部" };
        let mut value = [0.; 3];
        for (axis, name) in ["x", "y", "z"].iter().enumerate() {
            let prefix = format!("/avatar/parameters/VBS/Ref/{probe}/{kind}/{name}");
            let (Some(magnitude), Some(positive)) = (
                self.fields.get(&prefix),
                self.fields.get(&(prefix.clone() + "+")),
            ) else {
                return Err(format!("{label}データの初期値を取得中"));
            };
            let magnitude = magnitude.value;
            let positive = positive.value;
            if !direction && magnitude == 0. {
                return Err(format!("{label}位置が取得範囲を超えました"));
            }
            // The contact calculator's metre range scales with the avatar root.
            // Its normalized fields must use that same range to recover world metres.
            let radius = if direction {
                1.
            } else {
                1000. * self.probe_scale().unwrap_or(1.)
            };
            let d = (1. - magnitude) * radius;
            let near = if direction { 0.04 } else { 0.10 };
            let tolerance = if direction { 0.02 } else { 0.03 };
            let sign = &mut self.signs[base + axis];
            if positive != sign.contact {
                *sign = Sign {
                    contact: positive,
                    ..Sign::default()
                };
            }
            if d <= near {
                sign.near_at_ns = at;
            }
            let mismatch = positive > 0.
                && positive <= 2. * near / radius
                && d > near + tolerance
                && d < radius / 2.
                && 2. * (d / radius).min(1. - d / radius) - positive > 2. * tolerance / radius;
            if mismatch {
                if sign.mismatch_at_ns < 0 {
                    sign.mismatch_at_ns = at;
                }
                if at - sign.mismatch_at_ns >= 150_000_000
                    && sign.mismatch_at_ns - sign.near_at_ns <= 500_000_000
                {
                    sign.inferred = true;
                }
            } else {
                sign.mismatch_at_ns = -1;
            }
            if sign.inferred {
                inferred.push(prefix);
            }
            value[axis] = d * if positive > 0. && !sign.inferred {
                1.
            } else {
                -1.
            };
        }
        Ok(value)
    }
    fn probe_scale(&self) -> Option<f64> {
        self.fields
            .get("/avatar/parameters/ScaleFactor")
            .map(|f| f.value)
    }
    pub fn at(&mut self, at: i64) -> Result<PoseResult> {
        ensure!(at >= self.last_eval_ns, "Live pose time went backwards");
        self.last_eval_ns = at;
        let version = self
            .fields
            .get("/avatar/parameters/VBS/Ref/ProbeVersion")
            .map_or(0., |f| f.value);
        let direct =
            self.source_mode == "mouth_probe" || (self.source_mode == "auto" && version == 2.);
        let mut out = PoseResult {
            valid: false,
            relative_m: [0.; 3],
            mouth_m: [0.; 3],
            camera: self.camera.clone(),
            reason: String::new(),
            orientation: String::new(),
            source_model: if direct {
                "avatar_mouth_probe"
            } else {
                "avatar_head_offset_estimated"
            }
            .into(),
            details: json!({"epoch":self.epoch,"camera_policy":"hold_last_report","requested_source_mode":self.source_mode,"probe_version":version,"source_policy":if direct{"causal_mouth_hold_v1"}else{"causal_head_hold_v1"}}),
        };
        out.details["probe_scale_factor"] = json!(self.probe_scale());
        out.details["probe_scale_policy"] = json!(if self.probe_scale().is_some() {
            "observed_avatar_contact_range"
        } else {
            "legacy_unscaled_assumption_no_observation"
        });
        if self.scale_invalid {
            out.reason = "アバターの倍率データが不正です".into();
            return Ok(out);
        }
        if self
            .scale_changed_at_ns
            .is_some_and(|t| at - t < 100_000_000)
        {
            out.reason = "サイズ変更後の位置データを更新中".into();
            return Ok(out);
        }
        if self.source_mode == "auto" && version != 0. && version != 2. {
            out.source_model = "unsupported_probe".into();
            out.reason = "未対応のプローブ版です".into();
            return Ok(out);
        }
        if !self.camera_seen {
            out.reason = "カメラ待ち：撮影レンズ本体を一度動かしてください".into();
            return Ok(out);
        }
        let mut inferred = Vec::new();
        if direct {
            if self.mouth_signal_ns < 0 || at - self.mouth_signal_ns > 3_000_000_000 {
                out.reason = "口元データ待ち：修正版プローブとOSCを確認".into();
                return Ok(out);
            }
            let mut p = match self.vector("mouth", "p", 6, at, &mut inferred) {
                Ok(p) => p,
                Err(e) => {
                    out.reason = e;
                    return Ok(out);
                }
            };
            p[2] *= -1.;
            out.mouth_m = p;
            out.relative_m = relative(p, self.camera.position, self.camera.rotation);
            out.valid = true;
            out.reason = "カメラ連動中（口元プローブ・検証版）".into();
            out.orientation = "mouth_position_only".into();
            out.details["mouth_last_signal_ns"] = json!(self.mouth_signal_ns);
            out.details["inferred_sign_fields"] = json!(inferred);
            out.details["camera_observation_ns"] = json!(self.camera.time_ns);
            out.details["mouth_offset_applied"] = json!(false);
            return Ok(out);
        }
        if self.last_signal_ns < 0 || at - self.last_signal_ns > 1_000_000_000 {
            out.reason = "頭部データ待ち：OSCと取得アドオンを確認".into();
            return Ok(out);
        }
        let mut p = match self.vector("head", "p", 0, at, &mut inferred) {
            Ok(v) => v,
            Err(e) => {
                out.reason = e;
                return Ok(out);
            }
        };
        let f = match self.vector("head", "r", 3, at, &mut inferred) {
            Ok(v) => v,
            Err(e) => {
                out.reason = e;
                return Ok(out);
            }
        };
        if norm(f) < 0.1 {
            out.reason = "頭の向きが取得できません".into();
            return Ok(out);
        }
        let f = scale(f, 1. / norm(f));
        let mut q = unity_pose(
            &[
                0.,
                0.,
                0.,
                -f[1].clamp(-1., 1.).asin() * 180. / PI,
                f[0].atan2(f[2]) * 180. / PI,
                0.,
            ],
            at,
        )?
        .rotation;
        out.orientation = "zero_roll_fallback".into();
        if self.hmd_seen && at - self.hmd.time_ns <= 150_000_000 {
            let fh = rotate(self.hmd.rotation, [0., 0., -1.]);
            let up = rotate(self.hmd.rotation, [0., 1., 0.]);
            let fw = [f[0], f[1], -f[2]];
            let mut flat_h = [fh[0], 0., fh[2]];
            let mut flat_w = [fw[0], 0., fw[2]];
            if (fh[1].clamp(-1., 1.).asin() - fw[1].clamp(-1., 1.).asin()).abs() < 25. * PI / 180. {
                if norm(flat_h).min(norm(flat_w)) > 0.15 {
                    flat_h = scale(flat_h, 1. / norm(flat_h));
                    flat_w = scale(flat_w, 1. / norm(flat_w));
                    self.last_yaw = cross(flat_h, flat_w)[1].atan2(dot(flat_h, flat_w));
                    self.yaw_at_ns = at;
                }
                if at - self.yaw_at_ns <= 500_000_000 {
                    let mut hint = rotate(
                        [
                            0.,
                            (self.last_yaw / 2.).sin(),
                            0.,
                            (self.last_yaw / 2.).cos(),
                        ],
                        up,
                    );
                    hint = sub(hint, scale(fw, dot(hint, fw)));
                    if norm(hint) > 0.1 {
                        hint = scale(hint, 1. / norm(hint));
                        let roll = (-dot(hint, rotate(q, [1., 0., 0.])))
                            .atan2(dot(hint, rotate(q, [0., 1., 0.])));
                        q = product(q, [0., 0., (roll / 2.).sin(), (roll / 2.).cos()]);
                        out.orientation = "hmd_up_aligned_estimate".into();
                    }
                }
            }
        }
        p[2] *= -1.;
        out.mouth_m = add(
            p,
            rotate(
                q,
                scale(self.mouth_offset_m, self.probe_scale().unwrap_or(1.)),
            ),
        );
        out.relative_m = relative(out.mouth_m, self.camera.position, self.camera.rotation);
        out.valid = true;
        out.reason = "カメラ連動中（頭部から口元を推定）".into();
        out.details["inferred_sign_fields"] = json!(inferred);
        out.details["camera_observation_ns"] = json!(self.camera.time_ns);
        out.details["head_last_signal_ns"] = json!(self.last_signal_ns);
        out.details["head_rotation"] = json!(q);
        out.details["mouth_offset_applied"] = json!(true);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scalar(a: &str, v: f64) -> Message {
        Message {
            address: format!("/avatar/parameters/VBS/Ref/{a}"),
            typetag: ",f".into(),
            values: vec![json!(v)],
        }
    }
    #[test]
    fn stationary_camera_and_snapshot_epoch() {
        let mut p = LivePose::new([0.; 3], "auto").unwrap();
        let mut messages = vec![
            Message {
                address: "/usercamera/Pose".into(),
                typetag: ",ffffff".into(),
                values: vec![json!(0); 6],
            },
            Message {
                address: "/avatar/parameters/VBS/Ref/ProbeVersion".into(),
                typetag: ",i".into(),
                values: vec![json!(2)],
            },
        ];
        for (a, v) in [("x", 1.), ("y", 2.), ("z", 3.)] {
            messages.push(scalar(&format!("mouth/p/{a}"), 1. - v / 1000.));
            messages.push(scalar(&format!("mouth/p/{a}+"), 0.5));
        }
        p.feed(&messages, 100, false, 0).unwrap();
        let first = p.at(200).unwrap();
        assert!(first.valid);
        assert!((first.relative_m[0] - 1.).abs() < 1e-9);
        let checkpoint = p.checkpoint();
        let mut restored = LivePose::new([0., 0.1, 0.], "auto").unwrap();
        restored.restore(&checkpoint).unwrap();
        assert_eq!(
            serde_json::to_value(p.at(1_000_000_000).unwrap()).unwrap(),
            serde_json::to_value(restored.at(1_000_000_000).unwrap()).unwrap()
        );
        assert!(!p.at(3_000_000_101).unwrap().valid);
        p.feed(
            &[Message {
                address: "/avatar/change".into(),
                typetag: ",s".into(),
                values: vec![json!("new")],
            }],
            4_000_000_000,
            false,
            0,
        )
        .unwrap();
        p.feed(&messages, 4_100_000_000, true, 3_900_000_000)
            .unwrap();
        assert!(!p.at(4_100_000_000).unwrap().valid);
    }
    #[test]
    fn avatar_scale_restores_contact_world_units_and_survives_checkpoint() {
        let mut p = LivePose::new([0.; 3], "mouth_probe").unwrap();
        let factor = |v| Message {
            address: "/avatar/parameters/ScaleFactor".into(),
            typetag: ",f".into(),
            values: vec![json!(v)],
        };
        let mut messages = vec![
            Message {
                address: "/usercamera/Pose".into(),
                typetag: ",ffffff".into(),
                values: vec![json!(0); 6],
            },
            factor(1.),
        ];
        for (axis, v) in [("x", 2.), ("y", 1.), ("z", 3.)] {
            messages.push(scalar(&format!("mouth/p/{axis}"), 1. - v / 1000.));
            messages.push(scalar(&format!("mouth/p/{axis}+"), 0.5));
        }
        p.feed(&messages, 1_000_000_000, false, 0).unwrap();
        assert!(p.at(1_000_000_000).unwrap().valid);
        p.feed(&[factor(2.)], 1_100_000_000, false, 0).unwrap();
        assert!(!p.at(1_110_000_000).unwrap().valid);
        // After a scale change, world X/Z stay fixed while height doubles.
        p.feed(
            &[scalar("mouth/p/x", 0.999), scalar("mouth/p/z", 0.9985)],
            1_120_000_000,
            false,
            0,
        )
        .unwrap();
        // An HTTP response requested before the new UDP value cannot undo it.
        p.feed(&[factor(1.)], 1_150_000_000, true, 1_050_000_000)
            .unwrap();
        let checkpoint = p.checkpoint();
        let mut restored = LivePose::new([0.; 3], "mouth_probe").unwrap();
        restored.restore(&checkpoint).unwrap();
        let a = p.at(1_250_000_000).unwrap();
        let b = restored.at(1_250_000_000).unwrap();
        assert!(a.valid && norm(sub(a.mouth_m, [2., 2., -3.])) < 1e-9);
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap()
        );
        p.feed(&[factor(0.)], 1_300_000_000, false, 0).unwrap();
        assert!(!p.at(1_500_000_000).unwrap().valid);
    }
}
