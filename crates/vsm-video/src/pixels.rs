use anyhow::{Result, ensure};

/// Top-down BGRA to limited-range BT.709 NV12, with 2×2 chroma averaging.
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    ensure!(
        w > 0 && h > 0 && w % 2 == 0 && h % 2 == 0 && w <= 4096 && h <= 4096,
        "Invalid video dimensions"
    );
    ensure!(bgra.len() == w * h * 4, "Invalid BGRA frame size");
    let mut output = vec![0; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            let at = (y * w + x) * 4;
            let (b, g, r) = (bgra[at] as i32, bgra[at + 1] as i32, bgra[at + 2] as i32);
            output[y * w + x] =
                (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)).clamp(16, 235) as u8;
        }
    }
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for dy in 0..2 {
                for dx in 0..2 {
                    let at = ((y + dy) * w + x + dx) * 4;
                    b += bgra[at] as i32;
                    g += bgra[at + 1] as i32;
                    r += bgra[at + 2] as i32;
                }
            }
            let uv = w * h + (y / 2) * w + x;
            output[uv] = (128 + ((-26 * r - 87 * g + 112 * b + 512) >> 10)).clamp(16, 240) as u8;
            output[uv + 1] =
                (128 + ((112 * r - 102 * g - 10 * b + 512) >> 10)).clamp(16, 240) as u8;
        }
    }
    Ok(output)
}

pub fn synthetic(width: u32, height: u32, frame: u64, fps: u32) -> Vec<u8> {
    let mut pixels = vec![0; width as usize * height as usize * 4];
    let colors = [
        [255, 255, 255],
        [0, 255, 255],
        [255, 255, 0],
        [0, 255, 0],
        [255, 0, 255],
        [0, 0, 255],
        [255, 0, 0],
        [0, 0, 0],
    ];
    let marker = ((frame * 120 / fps as u64) % width as u64) as u32;
    for y in 0..height {
        for x in 0..width {
            let offset = (y as usize * width as usize + x as usize) * 4;
            let c = if x.abs_diff(marker) < 4 {
                [0, 0, 0]
            } else {
                colors[(x as usize * 8 / width as usize).min(7)]
            };
            pixels[offset..offset + 3].copy_from_slice(&c);
            pixels[offset + 3] = 255;
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn black_and_white_range() {
        let black = bgra_to_nv12(&[0, 0, 0, 255].repeat(4), 2, 2).unwrap();
        assert_eq!(black, [16, 16, 16, 16, 128, 128]);
        let white = bgra_to_nv12(&[255, 255, 255, 255].repeat(4), 2, 2).unwrap();
        assert_eq!(white[..4], [235; 4]);
        assert!(white[4..].iter().all(|v| (127..=129).contains(v)));
    }
    #[test]
    fn rejects_malformed_frame() {
        assert!(bgra_to_nv12(&[0; 15], 2, 2).is_err());
        assert!(bgra_to_nv12(&[0; 24], 3, 2).is_err());
    }
}
