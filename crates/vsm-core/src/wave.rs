use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub fn load_json(path: &Path) -> Result<Value> {
    let file = File::open(path).with_context(|| format!("Cannot read {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= 64 * 1024 * 1024,
        "Session JSON exceeds 64 MiB"
    );
    Ok(serde_json::from_reader(file.take(64 * 1024 * 1024 + 1))?)
}
pub fn save_json(path: &Path, value: &Value) -> Result<()> {
    let mut f = File::create(path)?;
    serde_json::to_writer_pretty(&mut f, value)?;
    f.write_all(b"\n")?;
    f.sync_data()?;
    Ok(())
}
pub fn sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn le(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0, |v, (i, &b)| v | ((b as u64) << (i * 8)))
}
pub fn header(format: &[u8], align: u16, frames: u64) -> Result<Vec<u8>> {
    let header_size = 12 + 36 + 8 + format.len() as u64 + 12 + 8;
    ensure!(
        align > 0
            && format.len() % 2 == 0
            && frames <= (i64::MAX as u64 - header_size) / align as u64,
        "Invalid streaming WAV size"
    );
    let bytes = frames * align as u64;
    let riff = bytes + header_size - 8;
    let rf64 = riff >= u32::MAX as u64;
    let mut b = Vec::new();
    b.extend(if rf64 { b"RF64" } else { b"RIFF" });
    b.extend((if rf64 { u32::MAX } else { riff as u32 }).to_le_bytes());
    b.extend(b"WAVE");
    b.extend(if rf64 { b"ds64" } else { b"JUNK" });
    b.extend(28u32.to_le_bytes());
    for v in [riff, bytes, frames] {
        b.extend((if rf64 { v } else { 0 }).to_le_bytes());
    }
    b.extend(0u32.to_le_bytes());
    b.extend(b"fmt ");
    b.extend((format.len() as u32).to_le_bytes());
    b.extend(format);
    b.extend(b"fact");
    b.extend(4u32.to_le_bytes());
    b.extend((if rf64 { u32::MAX } else { frames as u32 }).to_le_bytes());
    b.extend(b"data");
    b.extend((if rf64 { u32::MAX } else { bytes as u32 }).to_le_bytes());
    Ok(b)
}
pub fn float_format(channels: u16) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend(3u16.to_le_bytes());
    f.extend(channels.to_le_bytes());
    f.extend(48000u32.to_le_bytes());
    f.extend((48000 * channels as u32 * 4).to_le_bytes());
    f.extend((channels * 4).to_le_bytes());
    f.extend(32u16.to_le_bytes());
    f
}
pub struct WaveReader {
    file: File,
    data: u64,
    pub frames: u64,
    pub channels: u16,
    pub sample_rate: u32,
}
impl WaveReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut h = [0u8; 12];
        file.read_exact(&mut h)?;
        let rf64 = &h[..4] == b"RF64";
        ensure!(
            (rf64 || &h[..4] == b"RIFF") && &h[8..] == b"WAVE",
            "Expected RIFF/RF64 WAV"
        );
        ensure!(
            rf64 || le(&h[4..8]) + 8 == size,
            "Unsealed or truncated WAV"
        );
        let (mut fmt, mut data, mut ds) = (false, false, false);
        let (mut data_at, mut data_size, mut rf_data, mut rf_frames) = (0, 0, 0, 0);
        let (mut channels, mut sample_rate) = (0, 0);
        let mut at = 12;
        while at < size {
            ensure!(size - at >= 8, "Truncated WAV chunk");
            file.seek(SeekFrom::Start(at))?;
            let mut ch = [0u8; 8];
            file.read_exact(&mut ch)?;
            let mut len = le(&ch[4..]);
            if rf64 && &ch[..4] == b"data" && len == u32::MAX as u64 {
                ensure!(ds, "RF64 ds64 missing");
                len = rf_data;
            }
            ensure!(
                len <= size - at - 8
                    && len.checked_add(len & 1).is_some_and(|n| n <= size - at - 8),
                "Invalid WAV chunk length"
            );
            match &ch[..4] {
                b"ds64" => {
                    ensure!(rf64 && !ds && len >= 28, "Invalid RF64 ds64");
                    let mut b = [0u8; 28];
                    file.read_exact(&mut b)?;
                    ds = true;
                    ensure!(
                        le(&b[..8]) == size - 8 && le(&b[24..]) == 0,
                        "Unsupported or unsealed RF64"
                    );
                    rf_data = le(&b[8..16]);
                    rf_frames = le(&b[16..24]);
                }
                b"fmt " => {
                    ensure!(
                        !fmt && (16..=4096).contains(&len),
                        "Invalid WAV format chunk"
                    );
                    fmt = true;
                    let mut b = vec![0u8; len as usize];
                    file.read_exact(&mut b)?;
                    let mut kind = le(&b[..2]);
                    channels = le(&b[2..4]) as u16;
                    sample_rate = le(&b[4..8]) as u32;
                    if kind == 0xfffe {
                        ensure!(
                            len >= 40
                                && le(&b[16..18]) >= 22
                                && le(&b[18..20]) == 32
                                && b[24..40]
                                    == [
                                        3, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b,
                                        0x71
                                    ],
                            "Expected float32 subformat"
                        );
                        kind = 3;
                    }
                    ensure!(
                        kind == 3
                            && (1..=32).contains(&channels)
                            && sample_rate == 48000
                            && le(&b[14..16]) == 32
                            && le(&b[12..14]) == channels as u64 * 4
                            && le(&b[8..12]) == sample_rate as u64 * channels as u64 * 4,
                        "Session audio must be 48 kHz float32"
                    );
                }
                b"data" => {
                    ensure!(!data, "Duplicate WAV data");
                    data = true;
                    data_at = at + 8;
                    data_size = len;
                }
                _ => {}
            }
            at += 8 + len + (len & 1);
        }
        ensure!(
            fmt && data && data_size > 0 && (!rf64 || ds),
            "Missing or empty WAV data"
        );
        ensure!(data_size % (channels as u64 * 4) == 0, "Unaligned WAV");
        let frames = data_size / (channels as u64 * 4);
        ensure!(!rf64 || rf_frames == frames, "RF64 frame count mismatch");
        Ok(Self {
            file,
            data: data_at,
            frames,
            channels,
            sample_rate,
        })
    }
    pub fn read(&mut self, first: u64, count: u32) -> Result<Vec<f32>> {
        ensure!(
            first <= self.frames && count as u64 <= self.frames - first && count <= 480000,
            "Audio read outside recording"
        );
        let mut bytes = vec![0u8; count as usize * self.channels as usize * 4];
        self.file.seek(SeekFrom::Start(
            self.data + first * self.channels as u64 * 4,
        ))?;
        self.file.read_exact(&mut bytes)?;
        Ok(bytes
            .chunks_exact(4)
            .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
            .collect())
    }
}
pub struct WaveWriter {
    file: File,
    format: Vec<u8>,
    align: u16,
    pub frames: u64,
}
impl WaveWriter {
    pub fn create(path: &Path, channels: u16) -> Result<Self> {
        ensure!((1..=32).contains(&channels), "Invalid audio channels");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut result = Self {
            file,
            format: float_format(channels),
            align: channels * 4,
            frames: 0,
        };
        result.flush()?;
        Ok(result)
    }
    pub fn append(&mut self, samples: &[f32]) -> Result<()> {
        ensure!(
            samples.len() % (self.align as usize / 4) == 0,
            "Unaligned audio output"
        );
        let bytes: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.file.write_all(&bytes)?;
        self.frames += (bytes.len() / self.align as usize) as u64;
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        let h = header(&self.format, self.align, self.frames)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&h)?;
        self.file.seek(SeekFrom::Start(
            h.len() as u64 + self.frames * self.align as u64,
        ))?;
        self.file.flush()?;
        Ok(())
    }
    pub fn finish(mut self) -> Result<u64> {
        self.flush()?;
        self.file.sync_data()?;
        Ok(self.frames)
    }
}
impl Drop for WaveWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rf64_boundary() {
        let f = float_format(2);
        let h = header(&f, 8, 600_000_000).unwrap();
        assert_eq!(&h[..4], b"RF64");
        assert_eq!(le(&h[28..36]), 4_800_000_000);
        assert!(header(&f, 8, u64::MAX).is_err());
    }
}
