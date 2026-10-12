use crate::{
    AudioSink, Latest, Preview, Recorder, VideoConfig, VideoFrame, VideoReport, VideoSource,
    VideoStatus,
    flow::{
        AudioClock, AudioQueue, VideoPacer, VideoTimeline, audio_drain_deadline_ns,
        audio_output_bounds, audio_relative_time_ns, ns_to_frames, video_duration_100ns,
    },
    now_ns, pixels,
    spout::Receiver,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
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

struct CapturedPixels {
    pixels: Vec<u8>,
    acquired_qpc_ns: i64,
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
    fn read(&mut self, fps: u32) -> Result<Option<CapturedPixels>> {
        match self {
            Self::Spout(r) => Ok(r.read()?.map(|frame| CapturedPixels {
                pixels: frame.pixels,
                acquired_qpc_ns: frame.acquired_qpc_ns,
            })),
            Self::Synthetic(w, h, frame) => {
                let acquired_qpc_ns = now_ns();
                let pixels = pixels::synthetic(*w, *h, *frame, fps);
                *frame += 1;
                Ok(Some(CapturedPixels {
                    pixels,
                    acquired_qpc_ns,
                }))
            }
        }
    }
}
fn set_latest(latest: &Latest, width: u32, height: u32, frame: CapturedPixels) {
    *latest.lock().unwrap() = Some(VideoFrame {
        width,
        height,
        qpc_ns: frame.acquired_qpc_ns,
        bgra: Arc::new(frame.pixels),
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
                let started = Instant::now();
                let first = capture.read(30)?;
                let first_frames = u64::from(first.is_some());
                if let Some(pixels) = first {
                    set_latest(&worker_latest, width, height, pixels);
                }
                *worker_status.lock().unwrap() = VideoStatus {
                    active: true,
                    width,
                    height,
                    frames: first_frames,
                    max_video_processing_ns: started.elapsed().as_nanos().min(u64::MAX as u128)
                        as u64,
                    ..Default::default()
                };
                let _ = ready_tx.send(Ok(()));
                let period = Duration::from_nanos(1_000_000_000 / 30);
                while !worker_cancel.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let frame = capture.read(30)?;
                    let captured = frame.is_some();
                    if let Some(pixels) = frame {
                        set_latest(&worker_latest, width, height, pixels);
                    }
                    let elapsed = started.elapsed();
                    {
                        let mut status = worker_status.lock().unwrap();
                        status.frames += u64::from(captured);
                        status.max_video_processing_ns = status
                            .max_video_processing_ns
                            .max(elapsed.as_nanos().min(u64::MAX as u128) as u64);
                    }
                    // Include readback work in the 30fps budget instead of adding
                    // 33ms after every read, which caps a 4K preview near 20fps.
                    thread::sleep(period.saturating_sub(elapsed));
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

struct PendingVideo {
    bytes: Vec<u8>,
    time_100ns: i64,
    acquired_qpc_ns: i64,
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
    audio_clock: AudioClock,
    pending_video: Option<PendingVideo>,
    last_video_acquired_qpc_ns: i64,
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
            // Windows 8+ MPEG-4 sinks support files above 4GiB automatically;
            // no LARGE_MDAT/co64 opt-in exists. Preserve the default non-fragmented
            // sink rather than requesting MOOV-before-MDAT (an extra file copy).
            // https://learn.microsoft.com/windows/win32/medfound/mpeg-4-file-sink
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
                audio_clock: AudioClock::default(),
                pending_video: None,
                last_video_acquired_qpc_ns: 0,
                finalized: false,
            })
        }
    }
    fn video(&mut self, frame: &VideoFrame, time_100ns: i64) -> Result<()> {
        // Keep exactly one NV12 sample so its duration ends at the next actual
        // acquisition PTS. Frame rate gates work; it must not quantize timestamps.
        if let Some(previous) = self.pending_video.take() {
            let duration = video_duration_100ns(previous.time_100ns, time_100ns)?;
            self.write(self.video, &previous.bytes, previous.time_100ns, duration)?;
            self.last_video_acquired_qpc_ns = previous.acquired_qpc_ns;
        }
        let bytes = pixels::bgra_to_nv12(&frame.bgra, self.width, self.height)?;
        self.pending_video = Some(PendingVideo {
            bytes,
            time_100ns,
            acquired_qpc_ns: frame.qpc_ns,
        });
        self.video_frames += 1;
        Ok(())
    }
    fn finish_video(&mut self, cutoff_100ns: i64) -> Result<()> {
        if let Some(previous) = self.pending_video.take() {
            if cutoff_100ns <= previous.time_100ns {
                self.video_frames = self.video_frames.saturating_sub(1);
                return Ok(());
            }
            let duration = video_duration_100ns(previous.time_100ns, cutoff_100ns)?;
            self.write(self.video, &previous.bytes, previous.time_100ns, duration)?;
            self.last_video_acquired_qpc_ns = previous.acquired_qpc_ns;
        }
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
    fn audio(
        &mut self,
        samples: &[f32],
        at: i64,
        origin: i64,
        cutoff: Option<i64>,
        offset: i64,
    ) -> Result<()> {
        let available = samples.len() / 2;
        if available == 0 {
            return Ok(());
        }
        let span = self
            .audio_clock
            .advance(audio_relative_time_ns(at, origin, offset)?, available)?;
        let limit = cutoff.map(|at| ns_to_frames((at - origin).max(0)) as u64);
        let Some(bounds) = audio_output_bounds(span, self.audio_frames, limit) else {
            return Ok(());
        };
        let (start, end) = (bounds.start, bounds.end);
        self.silence_until(start)?;
        let count = (end - self.audio_frames) as usize;
        // Clip the source at recording boundaries, then interpolate very small
        // clock corrections across the packet. Never remove its first samples
        // or insert zeros merely because QPC jittered at a packet boundary.
        let full = (span.end - span.start).max(1) as f64;
        let source_start = ((self.audio_frames as i64 - span.start).max(0) as f64 / full
            * available as f64)
            .min(available as f64 - 1.);
        let source_end = ((end as i64 - span.start).max(0) as f64 / full * available as f64)
            .clamp(source_start + 1., available as f64)
            - 1.;
        let mut bytes = Vec::with_capacity(count * 4);
        for frame in 0..count {
            let position = if count == 1 {
                source_start
            } else {
                source_start + (source_end - source_start) * frame as f64 / (count - 1) as f64
            };
            let first = position.floor() as usize;
            let next = (first + 1).min(available - 1);
            let mix = (position - first as f64) as f32;
            for channel in 0..2 {
                let a = samples[first * 2 + channel];
                let b = samples[next * 2 + channel];
                let value = a + (b - a) * mix;
                let value = if value.is_finite() {
                    value.clamp(-1., 1.)
                } else {
                    0.
                };
                bytes.extend(((value * 32767.).round() as i16).to_le_bytes());
            }
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
            // Error recovery still seals any already retained final image. A
            // successful recording calls finish_video with its exact cutoff first.
            if let Some(previous) = self.pending_video.take() {
                self.write(
                    self.video,
                    &previous.bytes,
                    previous.time_100ns,
                    10_000_000 / self.fps as i64,
                )?;
                self.last_video_acquired_qpc_ns = previous.acquired_qpc_ns;
            }
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
    let queue = Arc::new(AudioQueue::default());
    let worker_queue = queue.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let latest = Arc::new(Mutex::new(None));
    let status = Arc::new(Mutex::new(VideoStatus::default()));
    let (worker_latest, worker_status) = (latest.clone(), status.clone());
    let path = path.to_path_buf();
    let worker = thread::Builder::new()
        .name("vsm-video-record".into())
        .spawn(move || {
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    path,
                    config,
                    &worker_queue,
                    &worker_latest,
                    &worker_status,
                    &ready_tx,
                )
            })) {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("Video recording worker panicked")),
            };
            worker_queue.close();
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
            sink: AudioSink { queue, status },
            latest,
            worker: Some(worker),
        }),
        other => {
            queue.stop_at(now_ns());
            let _ = worker.join();
            anyhow::bail!("Could not start video recording: {other:?}")
        }
    }
}
fn note_stop(
    queue: &AudioQueue,
    origin: i64,
    cutoff: &mut Option<i64>,
    drain_until: &mut i64,
    offset: i64,
) {
    if let Some(at) = queue.stop() {
        let at = at.max(origin);
        if cutoff.is_none_or(|previous| at < previous) {
            *cutoff = Some(at);
            *drain_until = audio_drain_deadline_ns(now_ns(), at, offset);
        }
    }
}

fn run(
    path: PathBuf,
    config: VideoConfig,
    queue: &AudioQueue,
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
    // Auto-origin follows this acquisition, so an already available image can
    // be held at PTS0. An explicit older fixture origin keeps the real positive
    // acquisition PTS instead of backdating startup through encoder warm-up.
    let initial = latest
        .lock()
        .unwrap()
        .clone()
        .context("Missing initial video frame")?;
    let first_video_acquired_qpc_ns = initial.qpc_ns;
    let mut timeline = VideoTimeline::new(origin);
    encoder.video(
        &initial,
        timeline.acquired(initial.qpc_ns.max(origin), None).unwrap(),
    )?;
    *status.lock().unwrap() = VideoStatus {
        active: true,
        width,
        height,
        origin_qpc_ns: origin,
        frames: encoder.video_frames,
        ..Default::default()
    };
    let _ = ready.send(Ok(()));
    let mut cutoff = None;
    let mut drain_until = 0;
    let mut repeated = 0;
    let mut failure = None;
    let mut pacer = VideoPacer {
        next: 1,
        skipped: 0,
    };
    let mut max_video_processing_ns = 0;
    loop {
        let now = now_ns();
        note_stop(
            queue,
            origin,
            &mut cutoff,
            &mut drain_until,
            config.audio_offset_ns,
        );
        if failure.is_none() {
            if let Some(error) = status.lock().unwrap().error.clone() {
                failure = Some(error);
                cutoff = Some(now);
                drain_until = audio_drain_deadline_ns(now, now, config.audio_offset_ns);
            }
        }
        // Drain available audio before expensive 4K readback/conversion/encode.
        // A slow video frame can reduce video fps, but cannot starve voice.
        let audio_started = Instant::now();
        let mut audio_waiting = false;
        while let Some((at, frames)) = queue.front_timing() {
            note_stop(
                queue,
                origin,
                &mut cutoff,
                &mut drain_until,
                config.audio_offset_ns,
            );
            if frames > 0
                && cutoff.is_none()
                && config.audio_offset_ns > 0
                && !encoder.audio_clock.due(
                    audio_relative_time_ns(at, origin, config.audio_offset_ns)?,
                    frames,
                    now_ns() - origin,
                )?
            {
                audio_waiting = true;
                break;
            }
            let packet = queue.pop().expect("Single audio queue consumer");
            encoder.audio(
                &packet.samples,
                packet.at,
                origin,
                cutoff,
                config.audio_offset_ns,
            )?;
            if audio_started.elapsed() >= Duration::from_millis(50) {
                break;
            }
        }
        let now = now_ns();
        if !cutoff.is_some_and(|at| now >= at)
            && pacer.due(origin, now, cutoff, config.fps).is_some()
        {
            let started = Instant::now();
            let captured = capture.read(config.fps);
            // Stop may have arrived while a texture mutex or GPU readback blocked.
            note_stop(
                queue,
                origin,
                &mut cutoff,
                &mut drain_until,
                config.audio_offset_ns,
            );
            match captured {
                Ok(Some(frame)) => {
                    if let Some(time) = timeline.acquired(frame.acquired_qpc_ns, cutoff) {
                        set_latest(latest, width, height, frame);
                        if let Some(frame) = latest.lock().unwrap().clone() {
                            encoder.video(&frame, time)?;
                        }
                    } else {
                        pacer.skipped += 1;
                    }
                }
                Ok(None) => {
                    repeated += 1;
                    // A repeat represents a hold at the current time. It does
                    // not pretend the old image was acquired again by Spout.
                    if let Some(time) = timeline.acquired(now_ns(), cutoff) {
                        if let Some(frame) = latest.lock().unwrap().clone() {
                            encoder.video(&frame, time)?;
                        }
                    } else {
                        pacer.skipped += 1;
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    status.lock().unwrap().error = Some(error.clone());
                    failure = Some(error);
                    let at = now_ns();
                    cutoff = Some(cutoff.map_or(at, |previous| previous.min(at)));
                    drain_until = audio_drain_deadline_ns(at, at, config.audio_offset_ns);
                }
            }
            max_video_processing_ns = max_video_processing_ns
                .max(started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        }
        let stats = queue.stats();
        {
            let mut state = status.lock().unwrap();
            state.frames = encoder.video_frames;
            state.audio_frames = encoder.audio_frames;
            state.repeated_frames = repeated;
            state.skipped_frames = pacer.skipped;
            state.queued_audio_frames = stats.frames;
            state.peak_audio_queue_frames = stats.peak;
            state.audio_queue_rejected_frames = stats.rejected;
            state.max_video_processing_ns = max_video_processing_ns;
            state.max_audio_jitter_frames = encoder.audio_clock.max_jitter_frames;
            state.audio_clock_adjustment_frames = encoder.audio_clock.adjusted_frames;
        }
        if cutoff.is_some_and(|at| now_ns() >= at) && now_ns() >= drain_until && stats.frames == 0 {
            break;
        }
        let next = pacer.next_ns(origin, config.fps);
        let wait = if cutoff.is_some_and(|at| now_ns() >= at) {
            Duration::from_millis(5)
        } else {
            Duration::from_nanos((next - now_ns()).clamp(0, 20_000_000) as u64)
        };
        if audio_waiting {
            queue.wait_until_due(wait);
        } else {
            queue.wait(wait);
        }
    }
    let end = cutoff.unwrap();
    let end_frames = ns_to_frames((end - origin).max(0)) as u64;
    encoder.silence_until(end_frames)?;
    encoder.finish_video((end - origin).max(0) / 100)?;
    encoder.finish()?;
    let stats = queue.stats();
    ensure!(
        stats.rejected == 0,
        "動画の音声保存キューが上限に達し、{:.3}秒の音声を保持できなかったため録画は失敗しました。音声はWAVから確認してください",
        stats.rejected as f64 / 48_000.
    );
    let warning = match (failure, pacer.skipped) {
        (Some(error), _) => Some(error),
        (None, skipped) if skipped > 0 => Some(format!(
            "映像処理が30fpsに追いつかず{skipped}フレームを間引きました。音声と収録時刻は維持しています"
        )),
        _ => None,
    };
    Ok(VideoReport {
        path: path.to_string_lossy().into_owned(),
        width,
        height,
        fps: config.fps,
        video_frames: encoder.video_frames,
        audio_frames: encoder.audio_frames,
        repeated_frames: repeated,
        skipped_frames: pacer.skipped,
        peak_audio_queue_frames: stats.peak,
        max_video_processing_ns,
        max_audio_jitter_frames: encoder.audio_clock.max_jitter_frames,
        audio_clock_adjustment_frames: encoder.audio_clock.adjusted_frames,
        origin_qpc_ns: origin,
        end_qpc_ns: end,
        audio_timestamp_basis: "capture_source_qpc_plus_manual_offset",
        audio_offset_ns: config.audio_offset_ns,
        video_timestamp_basis: "receiver_texture_acquisition_qpc_before_cpu_readback",
        first_video_acquired_qpc_ns,
        last_video_acquired_qpc_ns: encoder.last_video_acquired_qpc_ns,
        video_codec: "H.264",
        audio_codec: "AAC",
        warning,
    })
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
