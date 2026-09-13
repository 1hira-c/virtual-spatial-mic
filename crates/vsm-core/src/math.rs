use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub type Vec3 = [f64; 3];
pub type Quat = [f64; 4];
pub fn add(a: Vec3, b: Vec3) -> Vec3 {
    std::array::from_fn(|i| a[i] + b[i])
}
pub fn sub(a: Vec3, b: Vec3) -> Vec3 {
    std::array::from_fn(|i| a[i] - b[i])
}
pub fn scale(a: Vec3, s: f64) -> Vec3 {
    a.map(|v| v * s)
}
pub fn norm(a: Vec3) -> f64 {
    a[0].hypot(a[1]).hypot(a[2])
}
pub fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
pub fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
pub fn unit(q: Quat) -> Result<Quat> {
    ensure!(q.iter().all(|x| x.is_finite()), "Nonfinite quaternion");
    let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
    ensure!(n.is_finite() && n > 1e-12, "Invalid quaternion");
    Ok(q.map(|v| v / n))
}
pub fn conjugate(q: Quat) -> Quat {
    [-q[0], -q[1], -q[2], q[3]]
}
pub fn product(a: Quat, b: Quat) -> Quat {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}
pub fn rotate(q: Quat, v: Vec3) -> Vec3 {
    let p = product(product(q, [v[0], v[1], v[2], 0.]), conjugate(q));
    [p[0], p[1], p[2]]
}
pub fn relative(source: Vec3, listener: Vec3, rotation: Quat) -> Vec3 {
    rotate(conjugate(rotation), sub(source, listener))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pose {
    pub time_ns: i64,
    pub position: Vec3,
    pub rotation: Quat,
}
impl Default for Pose {
    fn default() -> Self {
        Self {
            time_ns: 0,
            position: [0.; 3],
            rotation: [0., 0., 0., 1.],
        }
    }
}
pub fn unity_pose(values: &[f64], time_ns: i64) -> Result<Pose> {
    ensure!(
        values.len() == 6 && values.iter().all(|v| v.is_finite()),
        "Pose requires six finite numbers"
    );
    let x = values[3] * std::f64::consts::PI / 360.;
    let y = values[4] * std::f64::consts::PI / 360.;
    let z = values[5] * std::f64::consts::PI / 360.;
    let q = product(
        product([0., y.sin(), 0., y.cos()], [x.sin(), 0., 0., x.cos()]),
        [0., 0., z.sin(), z.cos()],
    );
    Ok(Pose {
        time_ns,
        position: [values[0], values[1], -values[2]],
        rotation: [-q[0], -q[1], q[2], q[3]],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unity_camera_coordinates() {
        let p = unity_pose(&[2., 3., 4., 0., 90., 0.], 5).unwrap();
        let r = relative([3., 3., -4.], p.position, p.rotation);
        assert!(r[0].abs() < 1e-12 && r[1].abs() < 1e-12 && (r[2] + 1.).abs() < 1e-12);
        assert!(unit([0.; 4]).is_err());
    }
}
