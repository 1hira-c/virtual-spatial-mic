use serde_json::json;
use vsm_core::{osc::Message, pose::LivePose};

fn scalar(field: &str, value: f64) -> Message {
    Message {
        address: format!("/avatar/parameters/VBS/Ref/{field}"),
        typetag: ",f".into(),
        values: vec![json!(value)],
    }
}

#[test]
fn mouth_only_matches_three_point_tracking_and_survives_replay() {
    let mut minimal = LivePose::new([0.; 3], "auto").unwrap();
    let mut reference = minimal.clone();
    for frame in 0..20 {
        let time = (frame + 1) * 100_000_000;
        let position = [0.25 + frame as f64 * 0.025, 1.6, -0.5];
        let mut messages = vec![
            Message {
                address: "/avatar/parameters/VBS/Ref/ProbeVersion".into(),
                typetag: ",i".into(),
                values: vec![json!(2)],
            },
            Message {
                address: "/usercamera/Pose".into(),
                typetag: ",ffffff".into(),
                values: [0., 1.6, 0., 0., frame as f64 * 5., 0.]
                    .map(|v| json!(v))
                    .to_vec(),
            },
        ];
        for (axis, value) in ["x", "y", "z"].into_iter().zip(position) {
            messages.push(scalar(&format!("mouth/p/{axis}"), 1. - value.abs() / 1000.));
            messages.push(scalar(
                &format!("mouth/p/{axis}+"),
                if value > 0. { 2. * value / 1000. } else { 0. },
            ));
        }
        minimal.feed(&messages, time, false, 0).unwrap();
        for probe in ["head", "mouth", "root"] {
            for kind in ["p", "r"] {
                if probe == "mouth" && kind == "p" {
                    continue;
                }
                for axis in ["x", "y", "z"] {
                    messages.push(scalar(&format!("{probe}/{kind}/{axis}"), 0.99));
                    messages.push(scalar(&format!("{probe}/{kind}/{axis}+"), 0.5));
                }
            }
        }
        reference.feed(&messages, time, false, 0).unwrap();
        let a = minimal.at(time).unwrap();
        let b = reference.at(time).unwrap();
        assert!(a.valid, "{}", a.reason);
        assert!(b.valid, "{}", b.reason);
        assert_eq!(a.orientation, "mouth_position_only");
        assert_eq!(a.relative_m, b.relative_m);
        for (actual, expected) in
            a.mouth_m
                .into_iter()
                .zip([position[0], position[1], -position[2]])
        {
            assert!((actual - expected).abs() < 1e-8);
        }
        if frame == 9 {
            let checkpoint = minimal.checkpoint();
            minimal = LivePose::new([0.; 3], "auto").unwrap();
            minimal.restore(&checkpoint).unwrap();
        }
    }
    assert!(
        !minimal.at(6_000_000_000).unwrap().valid,
        "A disconnected module must become invalid"
    );
}
