use crate::{
    AudioSink, Command, Latest, Preview, Recorder, VideoConfig, VideoFrame, VideoReport,
    VideoSource, VideoStatus, now_ns, pixels, spout::Receiver,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver as Channel, RecvTimeoutError},
    },
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
    },
    core::PCWSTR,
};

struct Com;
impl Com {
    fn open() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct Media;
impl Media {
    fn open() -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        }
        Ok(Self)
    }
}
impl Drop for Media {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

enum Capture {
    Spout(Receiver),
    Synthetic(u32, u32, u64),
}
impl Capture {
    fn open(source: VideoSource) -> Result<Self> {
        match source {
            VideoSource::Spout { sender_name } => Ok(Self::Spout(Receiver::open(&sender_name)?)),
            VideoSource::Synthetic { width, height } => {
                ensure!(
                    width % 2 == 0
                        && height % 2 == 0
                        && (16..=4096).contains(&width)
                        && (16..=4096).contains(&height),
                    "Invalid synthetic resolution"
                );
                Ok(Self::Synthetic(width, height, 0))
            }
        }
    }
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Spout(r) => r.dimensions(),
            Self::Synthetic(w, h, _) => (*w, *h),
        }
    }
    fn read(&mut self, fps: u32) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Spout(r) => r.read(),
            Self::Synthetic(w, h, frame) => {
                let result = pixels::synthetic(*w, *h, *frame, fps);
                *frame += 1;
                Ok(Some(result))
            }
        }
    }
}
fn set_latest(latest: &Latest, width: u32, height: u32, bgra: Vec<u8>) {
    *latest.lock().unwrap() = Some(VideoFrame {
        width,
        height,
        qpc_ns: now_ns(),
        bgra: Arc::new(bgra),
    });
}

pub(super) fn preview(source: VideoSource) -> Result<Preview> {
    let latest = Arc::new(Mutex::new(None));
    let status = Arc::new(Mutex::new(VideoStatus::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (worker_latest, worker_status, worker_cancel) =
        (latest.clone(), status.clone(), cancel.clone());
    let worker = thread::Builder::new()
        .name("vsm-video-preview".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let _com = Com::open()?;
                let mut capture = Capture::open(source)?;
                let (width, height) = capture.dimensions();
                if let Some(pixels) = capture.read(30)? {
                    set_latest(&worker_latest, width, height, pixels);
                }
                *worker_status.lock().unwrap() = VideoStatus {
                    active: true,
                    width,
                    height,
                    ..Default::default()
                };
                let _ = ready_tx.send(Ok(()));
                while !worker_cancel.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(33));
                    if let Some(pixels) = capture.read(30)? {
                        set_latest(&worker_latest, width, height, pixels);
                    }
                }
                Ok(())
            })();
            let mut status = worker_status.lock().unwrap();
            status.active = false;
            if let Err(error) = result {
                status.error = Some(error.to_string());
                let _ = ready_tx.send(Err(error.to_string()));
            }
        })?;
    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(Preview {
            latest,
            status,
            cancel,
            worker: Some(worker),
        }),
        other => {
            cancel.store(true, Ordering::Relaxed);
            let _ = worker.join();
            anyhow::bail!("Could not start video preview: {other:?}")
        }
    }
}

struct Encoder {
    writer: IMFSinkWriter,
    video: u32,
    audio: u32,
    width: u32,
    height: u32,
    fps: u32,
    audio_frames: u64,
    video_frames: u64,
    finalized: bool,
}
impl Encoder {
    fn create(path: &Path, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let filename: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 3)?;
            let attributes = attributes.unwrap();
            attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            let writer = MFCreateSinkWriterFromURL(PCWSTR(filename.as_ptr()), None, &attributes)
                .context("Windows MP4 writer unavailable")?;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            video_attributes(&output, width, height, fps)?;
            let video = writer.AddStream(&output)?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            video_attributes(&input, width, height, fps)?;
            writer
                .SetInputMediaType(video, &input, None)
                .context("Windows H.264 encoder unavailable")?;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24000)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            let audio = writer.AddStream(&output)?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 192000)?;
            writer
                .SetInputMediaType(audio, &input, None)
                .context("Windows AAC encoder unavailable")?;
            writer.BeginWriting()?;
            Ok(Self {
                writer,
                video,
                audio,
                width,
                height,
                fps,
                audio_frames: 0,
                video_frames: 0,
                finalized: false,
            })
        }
    }
    fn video(&mut self, pixels: &[u8]) -> Result<()> {
        let bytes = pixels::bgra_to_nv12(pixels, self.width, self.height)?;
        let time = (self.video_frames as i128 * 10_000_000 / self.fps as i128) as i64;
        let end = ((self.video_frames + 1) as i128 * 10_000_000 / self.fps as i128) as i64;
        self.write(self.video, &bytes, time, end - time)?;
        self.video_frames += 1;
        Ok(())
    }
    fn audio_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let frames = bytes.len() / 4;
        let time = (self.audio_frames as i128 * 10_000_000 / 48000) as i64;
        let end = ((self.audio_frames + frames as u64) as i128 * 10_000_000 / 48000) as i64;
        self.write(self.audio, bytes, time, end - time)?;
        self.audio_frames += frames as u64;
        Ok(())
    }
    fn silence_until(&mut self, frame: u64) -> Result<()> {
        let zeros = [0u8; 4096 * 4];
        while self.audio_frames < frame {
            let count = (frame - self.audio_frames).min(4096) as usize;
            self.audio_bytes(&zeros[..count * 4])?;
        }
        Ok(())
    }
    fn audio(&mut self, samples: &[f32], at: i64, origin: i64, cutoff: Option<i64>) -> Result<()> {
        let first = ns_to_frames(at - origin);
        let available = samples.len() / 2;
        let skip = (self.audio_frames as i64 - first).max(0) as usize;
        if skip >= available {
            return Ok(());
        }
        let start = first + skip as i64;
        ensure!(start >= 0, "Invalid audio timeline");
        // Prevent a bogus timestamp from filling hours of silence or allocating huge buffers.
        ensure!(
            start as u64 <= self.audio_frames + 48000 * 10,
            "Video audio clock jumped more than 10 seconds"
        );
        let mut count = available - skip;
        if let Some(cutoff) = cutoff {
            let end = ns_to_frames((cutoff - origin).max(0));
            count = count.min((end - start).max(0) as usize);
        }
        if count == 0 {
            return Ok(());
        }
        self.silence_until(start as u64)?;
        let mut bytes = Vec::with_capacity(count * 4);
        for sample in &samples[skip * 2..(skip + count) * 2] {
            let sample = if sample.is_finite() {
                sample.clamp(-1., 1.)
            } else {
                0.
            };
            let value = (sample * 32767.).round() as i16;
            bytes.extend(value.to_le_bytes());
        }
        self.audio_bytes(&bytes)
    }
    fn write(&self, stream: u32, bytes: &[u8], time: i64, duration: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len().try_into()?)?;
            let mut pointer = std::ptr::null_mut();
            buffer.Lock(&mut pointer, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(stream, &sample)?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if !self.finalized {
            unsafe {
                self.writer.Finalize()?;
            }
            self.finalized = true;
        }
        Ok(())
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
unsafe fn video_attributes(
    media: &IMFMediaType,
    width: u32,
    height: u32,
    fps: u32,
) -> windows::core::Result<()> {
    unsafe {
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        media.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        media.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

pub(super) fn start(path: &Path, config: VideoConfig) -> Result<Recorder> {
    let (tx, rx) = mpsc::sync_channel(1024);
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let latest = Arc::new(Mutex::new(None));
    let status = Arc::new(Mutex::new(VideoStatus::default()));
    let (worker_latest, worker_status) = (latest.clone(), status.clone());
    let path = path.to_path_buf();
    let worker = thread::Builder::new()
        .name("vsm-video-record".into())
        .spawn(move || {
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(path, config, rx, &worker_latest, &worker_status, &ready_tx)
            })) {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("Video recording worker panicked")),
            };
            let mut state = worker_status.lock().unwrap();
            state.active = false;
            if let Err(error) = &result {
                state.error = Some(format!("{error:#}"));
                let _ = ready_tx.send(Err(format!("{error:#}")));
            }
            result
        })?;
    match ready_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(())) => Ok(Recorder {
            sink: AudioSink { tx, status },
            latest,
            worker: Some(worker),
        }),
        other => {
            let _ = tx.send(Command::Stop(now_ns()));
            let _ = worker.join();
            anyhow::bail!("Could not start video recording: {other:?}")
        }
    }
}
fn run(
    path: PathBuf,
    config: VideoConfig,
    rx: Channel<Command>,
    latest: &Latest,
    status: &Arc<Mutex<VideoStatus>>,
    ready: &mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<VideoReport> {
    let _com = Com::open()?;
    let _media = Media::open()?;
    let mut capture = Capture::open(config.source.clone())?;
    let (width, height) = capture.dimensions();
    let first = capture
        .read(config.fps)?
        .context("Spout映像がまだ到着していません。送信元のカメラを開いてください")?;
    set_latest(latest, width, height, first);
    let mut encoder = Encoder::create(&path, width, height, config.fps, config.bitrate)?;
    let origin = if config.start_qpc_ns <= 0 {
        if let Some(frame) = capture.read(config.fps)? {
            set_latest(latest, width, height, frame);
        }
        now_ns()
    } else {
        config.start_qpc_ns
    };
    *status.lock().unwrap() = VideoStatus {
        active: true,
        width,
        height,
        origin_qpc_ns: origin,
        ..Default::default()
    };
    let _ = ready.send(Ok(()));
    let mut cutoff = None;
    let mut drain_until = 0;
    let mut repeated = 0;
    let mut failure = None;
    loop {
        let now = now_ns();
        if failure.is_none() {
            if let Some(error) = status.lock().unwrap().error.clone() {
                failure = Some(error);
                cutoff = Some(now);
                drain_until = now + 350_000_000;
            }
        }
        let limit = cutoff.unwrap_or(now);
        let next =
            origin + (encoder.video_frames as i128 * 1_000_000_000 / config.fps as i128) as i64;
        if next < limit && next <= now {
            // An overdue PTS has no captured historic frame. Hold the cached
            // image rather than label a newly acquired future image as the past.
            if cutoff.is_some_and(|at| now >= at) || now - next >= 1_000_000_000 / config.fps as i64
            {
                repeated += 1;
            } else {
                match capture.read(config.fps) {
                    Ok(Some(frame)) => set_latest(latest, width, height, frame),
                    Ok(None) => repeated += 1,
                    Err(error) => {
                        let error = error.to_string();
                        status.lock().unwrap().error = Some(error.clone());
                        failure = Some(error);
                        cutoff = Some(now);
                        drain_until = now + 350_000_000;
                    }
                }
            }
            if let Some(frame) = latest.lock().unwrap().clone() {
                encoder.video(&frame.bgra)?;
            }
        }
        if cutoff.is_some() && now >= drain_until {
            break;
        }
        let next =
            origin + (encoder.video_frames as i128 * 1_000_000_000 / config.fps as i128) as i64;
        let wait = if cutoff.is_some() {
            Duration::from_millis(5)
        } else {
            Duration::from_nanos((next - now_ns()).clamp(0, 20_000_000) as u64)
        };
        match rx.recv_timeout(wait) {
            Ok(Command::Audio(samples, at)) => encoder.audio(&samples, at, origin, cutoff)?,
            Ok(Command::Stop(at)) => {
                if cutoff.is_none() {
                    cutoff = Some(at.max(origin));
                    drain_until = at.max(now_ns()) + 350_000_000;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if cutoff.is_none() {
                    cutoff = Some(now_ns());
                    drain_until = now_ns() + 350_000_000;
                }
            }
        }
        let mut state = status.lock().unwrap();
        state.frames = encoder.video_frames;
        state.audio_frames = encoder.audio_frames;
        state.repeated_frames = repeated;
    }
    let end = cutoff.unwrap();
    let end_frames = ns_to_frames((end - origin).max(0)) as u64;
    encoder.silence_until(end_frames)?;
    encoder.finish()?;
    Ok(VideoReport {
        path: path.to_string_lossy().into_owned(),
        width,
        height,
        fps: config.fps,
        video_frames: encoder.video_frames,
        audio_frames: encoder.audio_frames,
        repeated_frames: repeated,
        origin_qpc_ns: origin,
        end_qpc_ns: end,
        video_codec: "H.264",
        audio_codec: "AAC",
        warning: failure,
    })
}

fn ns_to_frames(ns: i64) -> i64 {
    let value = ns as i128 * 48000;
    ((value
        + if value >= 0 {
            500_000_000
        } else {
            -500_000_000
        })
        / 1_000_000_000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_rounding_preserves_256_frame_blocks() {
        for frame in (0..48_000 * 10).step_by(256) {
            let ns = frame as i64 * 1_000_000_000 / 48000;
            assert_eq!(ns_to_frames(ns), frame as i64);
        }
        assert_eq!(ns_to_frames(-1_000_000_000), -48_000);
    }
}

/// Read container-native stream types through Media Foundation (no external tool).
pub fn inspect(path: &Path) -> Result<(u32, u32, u32, u32)> {
    let _com = Com::open()?;
    let _media = Media::open()?;
    let filename: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let reader = MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), None)?;
        let video = reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)?;
        let audio = reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)?;
        ensure!(
            video.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_H264,
            "MP4 video is not H.264"
        );
        ensure!(
            audio.GetGUID(&MF_MT_SUBTYPE)? == MFAudioFormat_AAC,
            "MP4 audio is not AAC"
        );
        let size = video.GetUINT64(&MF_MT_FRAME_SIZE)?;
        Ok((
            (size >> 32) as u32,
            size as u32,
            audio.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?,
            audio.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?,
        ))
    }
}

#[derive(Debug, serde::Serialize)]
pub struct DecodeReport {
    pub video_frames: u64,
    pub video_changed_frames: u64,
    pub video_end_100ns: i64,
    pub audio_frames: u64,
    pub audio_nonzero_samples: u64,
    pub audio_end_100ns: i64,
}
/// Developer diagnostic: decode both streams with Windows built-in codecs.
pub fn decode_check(path: &Path) -> Result<DecodeReport> {
    let _com = Com::open()?;
    let _media = Media::open()?;
    let filename: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut report = DecodeReport {
        video_frames: 0,
        video_changed_frames: 0,
        video_end_100ns: 0,
        audio_frames: 0,
        audio_nonzero_samples: 0,
        audio_end_100ns: 0,
    };
    unsafe {
        for audio in [false, true] {
            let reader = MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), None)?;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            let stream = if audio {
                MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32
            } else {
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
            };
            reader.SetStreamSelection(stream, true)?;
            let media = MFCreateMediaType()?;
            media.SetGUID(
                &MF_MT_MAJOR_TYPE,
                if audio {
                    &MFMediaType_Audio
                } else {
                    &MFMediaType_Video
                },
            )?;
            media.SetGUID(
                &MF_MT_SUBTYPE,
                if audio {
                    &MFAudioFormat_PCM
                } else {
                    &MFVideoFormat_NV12
                },
            )?;
            if audio {
                media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48000)?;
            }
            reader.SetCurrentMediaType(stream, None, &media)?;
            let mut previous_hash = None;
            loop {
                let mut flags = 0;
                let mut time = 0;
                let mut sample = None;
                reader.ReadSample(
                    stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )?;
                if let Some(sample) = sample {
                    let end = time + sample.GetSampleDuration()?;
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut pointer = std::ptr::null_mut();
                    let mut length = 0;
                    buffer.Lock(&mut pointer, None, Some(&mut length))?;
                    let bytes = std::slice::from_raw_parts(pointer, length as usize);
                    if audio {
                        report.audio_frames += length as u64 / 4;
                        report.audio_nonzero_samples += bytes
                            .chunks_exact(2)
                            .filter(|sample| sample[0] != 0 || sample[1] != 0)
                            .count() as u64;
                        report.audio_end_100ns = end;
                    } else {
                        report.video_frames += 1;
                        report.video_end_100ns = end;
                        let mut hash = 14695981039346656037u64;
                        for value in bytes {
                            hash = (hash ^ *value as u64).wrapping_mul(1099511628211);
                        }
                        if previous_hash.is_some_and(|prior| prior != hash) {
                            report.video_changed_frames += 1;
                        }
                        previous_hash = Some(hash);
                    }
                    buffer.Unlock()?;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
        }
    }
    Ok(report)
}
