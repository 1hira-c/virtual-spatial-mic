use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub address: String,
    pub typetag: String,
    pub values: Vec<Value>,
}
fn string(bytes: &[u8], at: &mut usize) -> Result<String> {
    let first = *at;
    while *at < bytes.len() && bytes[*at] != 0 {
        *at += 1;
    }
    ensure!(*at < bytes.len(), "Unterminated OSC string");
    let text = std::str::from_utf8(&bytes[first..*at])?.to_owned();
    let end = (*at + 4) & !3;
    ensure!(end <= bytes.len(), "Short OSC padding");
    ensure!(
        bytes[*at..end].iter().all(|&b| b == 0),
        "Invalid OSC padding"
    );
    *at = end;
    Ok(text)
}
fn word(bytes: &[u8], at: &mut usize) -> Result<u32> {
    ensure!(*at + 4 <= bytes.len(), "Short OSC word");
    let v = u32::from_be_bytes(bytes[*at..*at + 4].try_into()?);
    *at += 4;
    Ok(v)
}
fn decode(bytes: &[u8], result: &mut Vec<Message>, depth: usize) -> Result<()> {
    ensure!(
        depth <= 8 && !bytes.is_empty() && bytes.len() <= 65536 && result.len() < 4096,
        "OSC bounds exceeded"
    );
    let mut at = 0;
    let address = string(bytes, &mut at)?;
    if address == "#bundle" {
        ensure!(bytes.len() >= 16, "Short OSC bundle");
        at = 16;
        while at < bytes.len() {
            let n = word(bytes, &mut at)? as usize;
            ensure!(n > 0 && n <= bytes.len() - at, "Invalid bundle length");
            decode(&bytes[at..at + n], result, depth + 1)?;
            at += n;
        }
        return Ok(());
    }
    ensure!(address.starts_with('/'), "Invalid OSC address");
    let typetag = string(bytes, &mut at)?;
    ensure!(typetag.starts_with(','), "OSC typetag missing");
    let mut values = Vec::new();
    for tag in typetag.bytes().skip(1) {
        values.push(match tag {
            b'f' => {
                let f = f32::from_bits(word(bytes, &mut at)?);
                ensure!(f.is_finite(), "Nonfinite OSC float");
                Value::from(f)
            }
            b'i' => Value::from(word(bytes, &mut at)? as i32),
            b's' => Value::from(string(bytes, &mut at)?),
            b'T' => Value::Bool(true),
            b'F' => Value::Bool(false),
            _ => bail!("Unsupported OSC typetag; raw retained"),
        });
    }
    ensure!(at == bytes.len(), "Trailing OSC data");
    result.push(Message {
        address,
        typetag,
        values,
    });
    Ok(())
}
pub fn decode_osc(bytes: &[u8]) -> Result<Vec<Message>> {
    let mut result = Vec::new();
    decode(bytes, &mut result, 0)?;
    Ok(result)
}
pub fn live_types() -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for (address, typetag) in [
        ("/usercamera/Pose", "ffffff"),
        ("/tracking/vrsystem/head/pose", "ffffff"),
        ("/tracking/vrsystem/leftwrist/pose", "ffffff"),
        ("/tracking/vrsystem/rightwrist/pose", "ffffff"),
        ("/avatar/change", "s"),
        ("/usercamera/Mode", "i"),
        ("/avatar/parameters/VBS/Ref/ProbeVersion", "i"),
    ] {
        result.insert(address.into(), typetag.into());
    }
    for name in [
        "ScaleFactor",
        "ScaleFactorInverse",
        "EyeHeightAsMeters",
        "EyeHeightAsPercent",
        "VelocityX",
        "VelocityY",
        "VelocityZ",
        "VelocityMagnitude",
        "AngularY",
        "Upright",
    ] {
        result.insert(format!("/avatar/parameters/{name}"), "f".into());
    }
    for name in ["ScaleModified", "Grounded", "InStation", "Seated", "AFK"] {
        result.insert(format!("/avatar/parameters/{name}"), "T".into());
    }
    for name in ["VRMode", "TrackingType"] {
        result.insert(format!("/avatar/parameters/{name}"), "i".into());
    }
    for name in [
        "Lock",
        "SmoothMovement",
        "LookAtMe",
        "AutoLevelRoll",
        "AutoLevelPitch",
        "Flying",
        "Streaming",
    ] {
        result.insert(format!("/usercamera/{name}"), "T".into());
    }
    for name in ["SmoothingStrength", "LookAtMeXOffset", "LookAtMeYOffset"] {
        result.insert(format!("/usercamera/{name}"), "f".into());
    }
    for probe in ["head", "mouth", "root"] {
        for kind in ["p", "r"] {
            for axis in ["x", "y", "z"] {
                for sign in ["", "+"] {
                    result.insert(
                        format!("/avatar/parameters/VBS/Ref/{probe}/{kind}/{axis}{sign}"),
                        "f".into(),
                    );
                }
            }
        }
        result.insert(
            format!("/avatar/parameters/VBS/Ref/{probe}/SaveObject"),
            "T".into(),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_packets_are_atomic() {
        assert!(decode_osc(b"/a\0\0,f\0\0\x7f\xc0\0\0").is_err());
        assert!(decode_osc(b"/a\0\x01,T\0\0").is_err());
        assert!(decode_osc(b"/a\0\0,T\0\0\0").is_err());
        let m = decode_osc(b"/a\0\0,T\0\0").unwrap();
        assert_eq!(m[0].values, vec![Value::Bool(true)]);
    }
}
