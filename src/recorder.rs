// ABOUTME: Records audio from the default input device to a temporary WAV file.
// ABOUTME: Uses cpal for audio capture and hound for WAV encoding at 16kHz mono 16-bit.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::audio_signal::{SignalMonitor, SignalSnapshot};
use crate::config::AudioConfig;

/// Preview audio buffered ahead of the consumer. Deep enough that a preview
/// transcription running for several seconds does not starve the stream, small
/// enough that a stalled consumer cannot grow memory without bound.
const PREVIEW_CHANNEL_CAPACITY: usize = 512;
/// Disk writes run on a dedicated worker. This queue absorbs normal storage
/// jitter without allowing an unhealthy disk to consume unbounded memory.
const WAV_WRITER_CHANNEL_CAPACITY: usize = 64;
/// Bounded preroll retained while the realtime provider connects. At common
/// CPAL chunk sizes this covers setup latency without accepting unbounded RAM.
const REALTIME_CHANNEL_CAPACITY: usize = 1024;
const MAX_TAIL_CAPTURE_MS: u32 = 5_000;
const MAX_BATCH_WAV_BYTES: u64 = 64 * 1024 * 1024;
const WAV_HEADER_BUDGET_BYTES: u64 = 4 * 1024;
const PACTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const PACTL_RESTORE_QUEUE_CAPACITY: usize = 16;
static CAPTURE_START_ACTIVE: AtomicBool = AtomicBool::new(false);
static PACTL_RESTORE_WORKER: std::sync::OnceLock<
    Result<std::sync::mpsc::SyncSender<String>, String>,
> = std::sync::OnceLock::new();

async fn run_capture_start<T, F>(
    deadline: std::time::Duration,
    start: F,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    if CAPTURE_START_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(
            "A previous microphone startup is still unresponsive; restart the audio service or Voxkey before trying again"
                .into(),
        );
    }

    struct ActiveCaptureStart;
    impl Drop for ActiveCaptureStart {
        fn drop(&mut self) {
            CAPTURE_START_ACTIVE.store(false, Ordering::Release);
        }
    }

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let spawn = std::thread::Builder::new()
        .name("voxkey-capture-start".to_string())
        .spawn(move || {
            let _active = ActiveCaptureStart;
            let _ = result_tx.send(start());
        });
    if let Err(error) = spawn {
        CAPTURE_START_ACTIVE.store(false, Ordering::Release);
        return Err(std::io::Error::other(format!(
            "Could not start the microphone setup worker: {error}"
        ))
        .into());
    }

    match tokio::time::timeout(deadline, result_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("The microphone setup worker stopped before replying".into()),
        Err(_) => Err(format!(
            "Microphone startup timed out after {:.1}s; restart the audio service or choose another microphone",
            deadline.as_secs_f32()
        )
        .into()),
    }
}

/// Restores an output sink only when Voxkey changed it from unmuted to muted.
/// An already-muted sink belongs to the user and is left untouched.
struct SystemAudioMuteGuard {
    sink: Option<String>,
}

impl SystemAudioMuteGuard {
    async fn acquire() -> Result<Option<Self>, String> {
        let sink = pactl_stdout(&["get-default-sink"]).await?;
        let sink = sink.trim();
        if sink.is_empty() {
            return Err("pactl returned an empty default sink".to_string());
        }
        let mute = pactl_stdout(&["get-sink-mute", sink]).await?;
        if parse_pactl_mute(&mute).ok_or_else(|| format!("unrecognized pactl output: {mute:?}"))? {
            return Ok(None);
        }
        pactl_stdout(&["set-sink-mute", sink, "1"]).await?;
        tracing::info!("Muted system output while recording");
        Ok(Some(Self {
            sink: Some(sink.to_string()),
        }))
    }

    async fn restore(mut self) {
        let Some(sink) = self.sink.as_deref() else {
            return;
        };
        // Keep ownership armed across the await. If this future is cancelled,
        // Drop queues the exact same sink for the bounded fallback worker.
        // A failed first attempt also gets that one fallback retry.
        if restore_output_sink(sink).await {
            self.sink.take();
        }
    }
}

impl Drop for SystemAudioMuteGuard {
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            schedule_output_restore(sink);
        }
    }
}

async fn pactl_stdout(args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("pactl");
    command.args(args).env("LC_ALL", "C").kill_on_drop(true);
    let output = tokio::time::timeout(PACTL_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "pactl {} timed out after {}s",
                args.first().copied().unwrap_or("command"),
                PACTL_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| format!("failed to run pactl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pactl {} failed: {}",
            args.first().copied().unwrap_or("command"),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("pactl output was not UTF-8: {error}"))
}

fn schedule_output_restore(sink: String) {
    let worker = PACTL_RESTORE_WORKER.get_or_init(|| {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<String>(PACTL_RESTORE_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("voxkey-audio-restore".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!("Could not start output-restore runtime: {error}");
                        return;
                    }
                };
                while let Ok(sink) = receiver.recv() {
                    let _ = runtime.block_on(restore_output_sink(&sink));
                }
            })
            .map_err(|error| format!("Could not start output-restore worker: {error}"))?;
        Ok(sender)
    });
    let Ok(worker) = worker else {
        tracing::error!("{}", worker.as_ref().unwrap_err());
        return;
    };
    match worker.try_send(sink) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(sink)) => {
            tracing::error!(
                sink = ?sink,
                "Output-restore queue is full; unmute this sink in system sound settings"
            )
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(sink)) => {
            tracing::error!(
                sink = ?sink,
                "Output-restore worker stopped; unmute this sink in system sound settings"
            )
        }
    }
}

async fn restore_output_sink(sink: &str) -> bool {
    let mute = pactl_stdout(&["get-sink-mute", sink]).await;
    if matches!(mute.as_deref().ok().and_then(parse_pactl_mute), Some(false)) {
        return true;
    }
    if let Err(error) = &mute {
        // Voxkey owns this restore because it successfully muted the exact
        // sink. Failure to inspect must not suppress the unmute attempt.
        tracing::warn!("Could not inspect system output mute state; attempting restore: {error}");
    }
    match pactl_stdout(&["set-sink-mute", sink, "0"]).await {
        Ok(_) => {
            tracing::info!("Restored system output after recording");
            true
        }
        Err(error) => {
            tracing::error!(
                sink = ?sink,
                "Could not restore system output; unmute this sink in system sound settings: {error}"
            );
            false
        }
    }
}

fn parse_pactl_mute(output: &str) -> Option<bool> {
    match output.trim() {
        "Mute: yes" => Some(true),
        "Mute: no" => Some(false),
        _ => None,
    }
}

/// Whether a recording also publishes an auxiliary PCM stream for previews.
/// Capturing it costs an allocation inside the real-time audio callback, so
/// recordings that nobody previews skip the work entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewCapture {
    Enabled,
    Disabled,
}

fn report_streaming_capture_error(
    errors: &tokio::sync::watch::Sender<Option<String>>,
    error: String,
) {
    tracing::error!("Audio input error: {error}");
    errors.send_replace(Some(error));
}

fn publish_streaming_samples(
    audio_tx: &tokio::sync::mpsc::Sender<Arc<[i16]>>,
    capture_errors: &tokio::sync::watch::Sender<Option<String>>,
    data: Vec<i16>,
) {
    match audio_tx.try_reserve() {
        Ok(permit) => permit.send(data.into()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            if capture_errors.borrow().is_none() {
                report_streaming_capture_error(
                    capture_errors,
                    "Realtime audio buffer overflowed; refusing an incomplete transcript"
                        .to_string(),
                );
            }
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
}

fn report_batch_capture_error(errors: &tokio::sync::watch::Sender<Option<String>>, error: String) {
    if errors.borrow().is_none() {
        tracing::error!("Audio input error: {error}");
        errors.send_replace(Some(error));
    }
}

fn queue_batch_samples(
    writer: &std::sync::mpsc::SyncSender<Arc<[i16]>>,
    capture_errors: &tokio::sync::watch::Sender<Option<String>>,
    recorded_samples: &AtomicU64,
    max_samples: u64,
    data: Vec<i16>,
) -> Option<Arc<[i16]>> {
    let prior_samples = recorded_samples.load(Ordering::Relaxed);
    if data.len() as u64 > max_samples.saturating_sub(prior_samples) {
        report_batch_capture_error(
            capture_errors,
            "Recording reached its configured duration or 64 MiB file limit".to_string(),
        );
        return None;
    }

    let sample_count = data.len() as u64;
    let chunk: Arc<[i16]> = data.into();
    match writer.try_send(chunk.clone()) {
        Ok(()) => {
            recorded_samples.fetch_add(sample_count, Ordering::Relaxed);
            Some(chunk)
        }
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            report_batch_capture_error(
                capture_errors,
                "Recording storage fell behind the microphone; refusing incomplete audio"
                    .to_string(),
            );
            None
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
            if capture_errors.borrow().is_none() {
                report_batch_capture_error(
                    capture_errors,
                    "Recording storage worker stopped unexpectedly".to_string(),
                );
            }
            None
        }
    }
}

fn write_i16_chunk<W: std::io::Write + std::io::Seek>(
    writer: &mut hound::WavWriter<W>,
    samples: &[i16],
) -> Result<(), hound::Error> {
    let count = u32::try_from(samples.len())
        .map_err(|_| hound::Error::FormatError("audio callback chunk is too large"))?;
    let mut output = writer.get_i16_writer(count);
    for sample in samples {
        output.write_sample(*sample);
    }
    output.flush()
}

fn run_wav_writer<W: std::io::Write + std::io::Seek>(
    mut writer: hound::WavWriter<W>,
    receiver: std::sync::mpsc::Receiver<Arc<[i16]>>,
    padding_samples: u64,
) -> Result<u64, String> {
    let mut recorded_samples = 0_u64;
    while let Ok(chunk) = receiver.recv() {
        write_i16_chunk(&mut writer, &chunk).map_err(|error| error.to_string())?;
        recorded_samples = recorded_samples.saturating_add(chunk.len() as u64);
    }

    let mut remaining = padding_samples;
    const ZERO_BLOCK: [i16; 8 * 1024] = [0; 8 * 1024];
    while remaining > 0 {
        let count = remaining.min(ZERO_BLOCK.len() as u64) as usize;
        write_i16_chunk(&mut writer, &ZERO_BLOCK[..count]).map_err(|error| error.to_string())?;
        remaining -= count as u64;
    }
    writer.finalize().map_err(|error| error.to_string())?;
    Ok(recorded_samples)
}

struct BatchWriter {
    sender: Option<std::sync::mpsc::SyncSender<Arc<[i16]>>>,
    thread: Option<std::thread::JoinHandle<Result<u64, String>>>,
}

impl BatchWriter {
    fn start(
        writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
        padding_samples: u64,
        capture_errors: tokio::sync::watch::Sender<Option<String>>,
    ) -> Result<Self, std::io::Error> {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<Arc<[i16]>>(WAV_WRITER_CHANNEL_CAPACITY);
        let thread = std::thread::Builder::new()
            .name("voxkey-wav-writer".to_string())
            .spawn(move || {
                let result = run_wav_writer(writer, receiver, padding_samples);
                if let Err(error) = &result {
                    report_batch_capture_error(&capture_errors, error.clone());
                }
                result
            })?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    fn sender(&self) -> std::sync::mpsc::SyncSender<Arc<[i16]>> {
        self.sender
            .as_ref()
            .expect("WAV writer sender already closed")
            .clone()
    }

    async fn finish(mut self) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        self.sender.take();
        let thread = self.thread.take().ok_or("WAV writer task is missing")?;
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|error| std::io::Error::other(format!("WAV writer join failed: {error}")))?
            .map_err(|_| std::io::Error::other("WAV writer thread panicked"))?
            .map_err(|error| std::io::Error::other(format!("Could not write recording: {error}")))
            .map_err(Into::into)
    }
}

fn max_batch_samples(sample_rate: u32, channels: u16, max_seconds: u32) -> u64 {
    let duration_samples = u64::from(sample_rate)
        .saturating_mul(u64::from(channels))
        .saturating_mul(u64::from(max_seconds));
    let padding_samples = transcription_padding_samples(sample_rate, channels);
    let file_samples = (MAX_BATCH_WAV_BYTES.saturating_sub(WAV_HEADER_BUDGET_BYTES)
        / std::mem::size_of::<i16>() as u64)
        .saturating_sub(padding_samples);
    duration_samples.min(file_samples)
}

fn transcription_padding_samples(sample_rate: u32, channels: u16) -> u64 {
    let samples = u128::from(sample_rate)
        .saturating_mul(u128::from(channels))
        .saturating_mul(crate::preview::TRAILING_SILENCE.as_millis())
        / 1_000;
    samples.min(u128::from(u64::MAX)) as u64
}

fn publish_preview_chunk(
    preview_tx: Option<&tokio::sync::mpsc::Sender<Arc<[i16]>>>,
    dropped_preview_chunks: &AtomicU64,
    chunk: Arc<[i16]>,
) {
    let Some(preview_tx) = preview_tx else {
        return;
    };
    match preview_tx.try_send(chunk) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            dropped_preview_chunks.fetch_add(1, Ordering::Relaxed);
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
}

fn selected_input_config(
    device: &cpal::Device,
    target_rate: u32,
    target_channels: u16,
) -> Result<cpal::SupportedStreamConfig, Box<dyn std::error::Error + Send + Sync>> {
    match device.supported_input_configs() {
        Ok(configs) => {
            crate::audio_adapter::select_input_config(configs, target_rate, target_channels)
                .ok_or_else(|| "The selected microphone reports no supported input format".into())
        }
        Err(error) => {
            tracing::warn!(
                "Could not enumerate microphone formats ({error}); using its default format"
            );
            Ok(device.default_input_config()?)
        }
    }
}

fn build_adapted_input_stream<F, E>(
    device: &cpal::Device,
    target_rate: u32,
    target_channels: u16,
    recording: Arc<AtomicBool>,
    mut consume: F,
    report_error: E,
) -> Result<cpal::Stream, Box<dyn std::error::Error + Send + Sync>>
where
    F: FnMut(Vec<i16>) + Send + 'static,
    E: Fn(String) + Clone + Send + 'static,
{
    let native = selected_input_config(device, target_rate, target_channels)?;
    let native_rate = native.sample_rate().0;
    let native_channels = native.channels();
    let native_format = native.sample_format();
    let stream_config: cpal::StreamConfig = native.into();
    tracing::info!(
        native_rate,
        native_channels,
        native_format = %native_format,
        target_rate,
        target_channels,
        "Configured microphone format adapter"
    );

    let mut adapter = crate::audio_adapter::AudioAdapter::new(
        native_rate,
        native_channels,
        target_rate,
        target_channels,
    )?;
    let callback_error = report_error.clone();
    let stream = device.build_input_stream_raw(
        &stream_config,
        native_format,
        move |data, _| {
            if !recording.load(Ordering::Relaxed) {
                return;
            }
            match adapter.process_data(data, native_format) {
                Ok(samples) if !samples.is_empty() => consume(samples),
                Ok(_) => {}
                Err(error) => callback_error(error),
            }
        },
        move |error| report_error(error.to_string()),
        None,
    )?;
    Ok(stream)
}

/// Records audio from the default input device.
#[derive(Clone)]
pub struct Recorder {
    sample_rate: u32,
    channels: u16,
    tail_capture: std::time::Duration,
    input_device: String,
    mute_output_while_recording: bool,
    max_recording_seconds: u32,
}

impl Recorder {
    pub fn new(config: &AudioConfig) -> Self {
        let tail_capture_ms = config.tail_capture_ms.min(MAX_TAIL_CAPTURE_MS);
        if tail_capture_ms != config.tail_capture_ms {
            tracing::warn!(
                "Tail capture of {}ms exceeds the {}ms limit; clamping it",
                config.tail_capture_ms,
                MAX_TAIL_CAPTURE_MS
            );
        }
        Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
            tail_capture: std::time::Duration::from_millis(tail_capture_ms as u64),
            input_device: config.input_device.clone(),
            mute_output_while_recording: config.mute_output_while_recording,
            max_recording_seconds: config.max_recording_seconds.clamp(1, 3_600),
        }
    }

    fn resolve_input_device(
        &self,
    ) -> Result<cpal::Device, Box<dyn std::error::Error + Send + Sync>> {
        let host = cpal::default_host();

        if !self.input_device.is_empty() {
            if let Some(device) = host
                .input_devices()?
                .find(|device| device.name().is_ok_and(|name| name == self.input_device))
            {
                return Ok(device);
            }
            return Err(format!(
                "Configured audio input '{}' is unavailable; select another microphone or choose the system default",
                self.input_device
            )
            .into());
        }

        host.default_input_device()
            .ok_or_else(|| "No default input device available".into())
    }

    /// Start streaming audio capture. Returns a handle with a channel receiver
    /// that delivers raw PCM i16 chunks for real-time processing.
    pub async fn start_streaming(
        &self,
    ) -> Result<StreamingRecordingHandle, Box<dyn std::error::Error + Send + Sync>> {
        let recorder = self.clone();
        let system_audio_mute = self.acquire_system_audio_mute().await;
        let mut handle = run_capture_start(crate::deadline::AUDIO_CAPTURE_START, move || {
            recorder.start_streaming_capture()
        })
        .await?;
        handle.system_audio_mute = system_audio_mute;
        Ok(handle)
    }

    fn start_streaming_capture(
        &self,
    ) -> Result<StreamingRecordingHandle, Box<dyn std::error::Error + Send + Sync>> {
        let device = self.resolve_input_device()?;

        tracing::info!("Streaming from: {}", device.name().unwrap_or_default());

        let (tx, rx) = tokio::sync::mpsc::channel::<Arc<[i16]>>(REALTIME_CHANNEL_CAPACITY);
        let (capture_error_tx, capture_error_rx) = tokio::sync::watch::channel(None);
        let buffer_errors = capture_error_tx.clone();
        let callback_errors = capture_error_tx.clone();

        let recording = Arc::new(AtomicBool::new(true));
        let signal = SignalMonitor::default();
        let callback_signal = signal.clone();

        let stream = build_adapted_input_stream(
            &device,
            self.sample_rate,
            self.channels,
            recording.clone(),
            move |data| {
                callback_signal.observe(&data);
                publish_streaming_samples(&tx, &buffer_errors, data);
            },
            move |error| {
                report_streaming_capture_error(&callback_errors, error);
            },
        )?;

        stream.play()?;
        tracing::info!("Streaming recording started");

        Ok(StreamingRecordingHandle {
            stream: Some(stream),
            recording,
            signal,
            rx: Some(rx),
            capture_error_rx: Some(capture_error_rx),
            _capture_error_tx: capture_error_tx,
            system_audio_mute: None,
        })
    }

    /// Start recording. Returns a handle that can be used to stop recording.
    pub async fn start(
        &self,
        preview_capture: PreviewCapture,
    ) -> Result<RecordingHandle, Box<dyn std::error::Error + Send + Sync>> {
        let recorder = self.clone();
        let system_audio_mute = self.acquire_system_audio_mute().await;
        let mut handle = run_capture_start(crate::deadline::AUDIO_CAPTURE_START, move || {
            recorder.start_batch_capture(preview_capture)
        })
        .await?;
        handle.system_audio_mute = system_audio_mute;
        Ok(handle)
    }

    fn start_batch_capture(
        &self,
        preview_capture: PreviewCapture,
    ) -> Result<RecordingHandle, Box<dyn std::error::Error + Send + Sync>> {
        let device = self.resolve_input_device()?;

        tracing::info!("Recording from: {}", device.name().unwrap_or_default());

        // Stays auto-deleting until the whole capture pipeline is running, so
        // a device that refuses to start leaves no orphaned file behind. The
        // handle owns it from then on and only `stop()` hands it onward.
        let audio_file = tempfile::Builder::new()
            .prefix("voxkey_")
            .suffix(".wav")
            .tempfile()?
            .into_temp_path();
        let wav_path = audio_file.to_path_buf();

        let spec = hound::WavSpec {
            channels: self.channels,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&wav_path, spec)?;
        let (preview_tx, preview_rx) = match preview_capture {
            PreviewCapture::Enabled => {
                let (tx, rx) = tokio::sync::mpsc::channel::<Arc<[i16]>>(PREVIEW_CHANNEL_CAPACITY);
                (Some(tx), Some(rx))
            }
            PreviewCapture::Disabled => (None, None),
        };
        let dropped_preview_chunks = Arc::new(AtomicU64::new(0));
        let dropped_preview_chunks_clone = dropped_preview_chunks.clone();

        let recording = Arc::new(AtomicBool::new(true));
        let signal = SignalMonitor::default();
        let callback_signal = signal.clone();
        let (capture_error_tx, capture_error_rx) = tokio::sync::watch::channel(None);
        let writer = BatchWriter::start(
            writer,
            transcription_padding_samples(self.sample_rate, self.channels),
            capture_error_tx.clone(),
        )?;
        let callback_writer = writer.sender();
        let sample_capture_errors = capture_error_tx.clone();
        let stream_capture_errors = capture_error_tx.clone();
        let recorded_samples = Arc::new(AtomicU64::new(0));
        let callback_recorded_samples = recorded_samples.clone();
        let max_samples =
            max_batch_samples(self.sample_rate, self.channels, self.max_recording_seconds);

        let stream = build_adapted_input_stream(
            &device,
            self.sample_rate,
            self.channels,
            recording.clone(),
            move |data| {
                callback_signal.observe(&data);
                let Some(chunk) = queue_batch_samples(
                    &callback_writer,
                    &sample_capture_errors,
                    &callback_recorded_samples,
                    max_samples,
                    data,
                ) else {
                    return;
                };
                // Preview transcription is best-effort: it must never block the
                // audio callback or compromise the final WAV. A full channel
                // drops the chunk, which leaves a gap in preview audio only.
                publish_preview_chunk(preview_tx.as_ref(), &dropped_preview_chunks_clone, chunk);
            },
            move |error| {
                report_batch_capture_error(&stream_capture_errors, error);
            },
        )?;

        stream.play()?;
        tracing::info!("Recording started");

        Ok(RecordingHandle {
            stream: Some(stream),
            writer: Some(writer),
            recording,
            signal,
            wav_path: Some(audio_file.keep()?),
            tail_capture: self.tail_capture,
            preview_rx,
            dropped_preview_chunks,
            recorded_samples,
            capture_error_rx: Some(capture_error_rx),
            _capture_error_tx: capture_error_tx,
            system_audio_mute: None,
        })
    }

    async fn acquire_system_audio_mute(&self) -> Option<SystemAudioMuteGuard> {
        if !self.mute_output_while_recording {
            return None;
        }
        match SystemAudioMuteGuard::acquire().await {
            Ok(guard) => guard,
            Err(error) => {
                tracing::warn!("Could not mute system output for recording: {error}");
                None
            }
        }
    }
}

/// Enumerate input device names for the settings application. CPAL exposes
/// names rather than stable cross-backend IDs, which is also what the recorder
/// can reliably resolve on Linux.
pub fn available_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    normalize_input_device_names(devices.filter_map(|device| device.name().ok()).collect())
}

fn normalize_input_device_names(mut names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    names.sort_by_key(|name| name.to_lowercase());
    names
}

/// Handle to a streaming audio capture. Provides a channel of raw PCM chunks.
pub struct StreamingRecordingHandle {
    stream: Option<cpal::Stream>,
    recording: Arc<AtomicBool>,
    signal: SignalMonitor,
    rx: Option<tokio::sync::mpsc::Receiver<Arc<[i16]>>>,
    capture_error_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    _capture_error_tx: tokio::sync::watch::Sender<Option<String>>,
    system_audio_mute: Option<SystemAudioMuteGuard>,
}

impl StreamingRecordingHandle {
    pub fn signal_snapshot(&self) -> SignalSnapshot {
        self.signal.snapshot()
    }

    /// Take the audio chunk receiver. Can only be called once.
    pub fn take_rx(&mut self) -> Option<tokio::sync::mpsc::Receiver<Arc<[i16]>>> {
        self.rx.take()
    }

    /// Take the capture-error stream. Can only be called once.
    pub fn take_capture_error_rx(
        &mut self,
    ) -> Option<tokio::sync::watch::Receiver<Option<String>>> {
        self.capture_error_rx.take()
    }

    /// Stop audio production without waiting for secondary cleanup. Teardown
    /// uses this before entering its shared asynchronous deadline.
    pub fn stop_capture(&mut self) {
        self.recording.store(false, Ordering::Relaxed);
        drop(self.stream.take());
    }

    /// Complete secondary cleanup after capture has stopped.
    pub async fn restore_system_audio(&mut self) {
        if let Some(guard) = self.system_audio_mute.take() {
            guard.restore().await;
        }
        tracing::info!("Streaming recording stopped");
    }
}

/// Handle to an in-progress recording. Call `stop()` to finalize the WAV file.
pub struct RecordingHandle {
    stream: Option<cpal::Stream>,
    writer: Option<BatchWriter>,
    recording: Arc<AtomicBool>,
    signal: SignalMonitor,
    /// The captured audio, owned by this handle until `stop()` succeeds and
    /// hands it to the transcriber. While it is still here, dropping the
    /// handle deletes it.
    wav_path: Option<PathBuf>,
    tail_capture: std::time::Duration,
    preview_rx: Option<tokio::sync::mpsc::Receiver<Arc<[i16]>>>,
    dropped_preview_chunks: Arc<AtomicU64>,
    recorded_samples: Arc<AtomicU64>,
    capture_error_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    _capture_error_tx: tokio::sync::watch::Sender<Option<String>>,
    system_audio_mute: Option<SystemAudioMuteGuard>,
}

impl RecordingHandle {
    pub fn signal_snapshot(&self) -> SignalSnapshot {
        self.signal.snapshot()
    }

    /// Take the auxiliary PCM stream used for replaceable transcription
    /// previews. The finalized WAV remains independent and lossless.
    pub fn take_preview_rx(&mut self) -> Option<tokio::sync::mpsc::Receiver<Arc<[i16]>>> {
        self.preview_rx.take()
    }

    /// Preview chunks dropped because the preview consumer fell behind. Any
    /// drop means the preview heard less audio than the saved WAV.
    pub fn preview_chunks_dropped(&self) -> u64 {
        self.dropped_preview_chunks.load(Ordering::Relaxed)
    }

    /// Take the capture-error stream. The daemon monitors it while recording
    /// so hot-unplug, callback failures, and resource limits fail promptly.
    pub fn take_capture_error_rx(
        &mut self,
    ) -> Option<tokio::sync::watch::Receiver<Option<String>>> {
        self.capture_error_rx.take()
    }

    /// Stop recording and finalize the WAV file. Returns the path to the WAV file.
    /// Captures a short tail of audio before stopping to avoid cutting off the last words.
    #[cfg(test)]
    pub async fn stop(self) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        self.stop_with_summary()
            .await
            .map(|recording| recording.path)
    }

    /// Finalize a recording and report only microphone samples, excluding the
    /// synthetic trailing silence appended for final transcription quality.
    #[cfg(test)]
    pub async fn stop_with_sample_count(
        self,
    ) -> Result<(PathBuf, u64), Box<dyn std::error::Error + Send + Sync>> {
        self.stop_with_summary()
            .await
            .map(|recording| (recording.path, recording.recorded_samples))
    }

    /// Finalize a recording and return the captured signal summary used by
    /// transcription guards, diagnostics, and History metrics.
    pub async fn stop_with_summary(
        mut self,
    ) -> Result<FinalizedRecording, Box<dyn std::error::Error + Send + Sync>> {
        // Keep capturing for the configured tail so trailing speech isn't cut off
        tokio::time::sleep(self.tail_capture).await;

        self.recording.store(false, Ordering::Relaxed);

        // Drop the stream to stop capturing
        drop(self.stream.take());
        if let Some(guard) = self.system_audio_mute.take() {
            guard.restore().await;
        }

        // Closing the callback-owned sender lets the disk worker drain every
        // accepted chunk, append transcription padding, and finalize off the
        // async runtime. No storage operation runs on CPAL's realtime thread.
        let writer = self.writer.take().ok_or("recording has no WAV writer")?;
        let written_samples = writer.finish().await;
        let capture_error = self._capture_error_tx.borrow().clone();
        if let Some(error) = capture_error {
            return Err(format!("Audio capture failed: {error}").into());
        }
        let recorded_samples = written_samples?;
        debug_assert_eq!(
            recorded_samples,
            self.recorded_samples.load(Ordering::Relaxed),
            "the callback admission count and WAV writer count diverged"
        );
        let signal = self.signal.snapshot();

        let dropped = self.dropped_preview_chunks.load(Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(
                "Preview audio lost {dropped} chunk(s) because the preview consumer fell behind; \
                 previews may have skipped words. The saved recording is unaffected."
            );
        }

        // Taken last: every step above can fail, and until the audio is handed
        // over this handle is still responsible for deleting it.
        let wav_path = self.wav_path.take().ok_or("recording has no audio file")?;
        tracing::info!("Recording stopped, saved to: {}", wav_path.display());
        Ok(FinalizedRecording {
            path: wav_path,
            recorded_samples,
            signal,
        })
    }

    /// Stop and delete an abandoned recording, waiting for owned system-audio
    /// state to be restored before the daemon reports Idle.
    pub async fn discard(mut self) {
        self.recording.store(false, Ordering::Relaxed);
        drop(self.stream.take());
        if let Some(guard) = self.system_audio_mute.take() {
            guard.restore().await;
        }
        // Drop retains ownership of scratch-file and writer cleanup.
    }
}

#[derive(Debug)]
pub struct FinalizedRecording {
    pub path: PathBuf,
    pub recorded_samples: u64,
    pub signal: SignalSnapshot,
}

impl Drop for RecordingHandle {
    /// A recording the daemon never stopped -- shutdown, a session restart, or
    /// a portal error mid-dictation -- must not leave whatever the microphone
    /// captured sitting in the temp directory.
    fn drop(&mut self) {
        let Some(wav_path) = self.wav_path.take() else {
            return;
        };

        self.recording.store(false, Ordering::Relaxed);
        drop(self.stream.take());
        if let Some(writer) = self.writer.as_mut() {
            writer.sender.take();
        }

        match std::fs::remove_file(&wav_path) {
            Ok(()) => tracing::info!(
                "Discarded the abandoned recording at {}",
                wav_path.display()
            ),
            Err(error) => tracing::warn!(
                "Failed to discard the abandoned recording at {}: {error}",
                wav_path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_capture_start_hits_its_deadline_without_blocking_the_controller() {
        let started = std::time::Instant::now();
        let result = run_capture_start(std::time::Duration::from_millis(25), || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(())
        })
        .await;

        let error = result.expect_err("a blocked microphone start must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "microphone startup held the controller for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn microphone_peak_is_normalized_and_handles_i16_min() {
        let signal = SignalMonitor::default();
        signal.observe(&[0, 8_192, -16_384]);
        assert!((signal.snapshot().latest_peak - 0.5).abs() < f64::EPSILON);

        signal.observe(&[i16::MIN]);
        assert!((signal.snapshot().latest_peak - 1.0).abs() < f64::EPSILON);
    }

    struct ToggleWriter {
        inner: std::io::Cursor<Vec<u8>>,
        fail: Arc<AtomicBool>,
    }

    impl std::io::Write for ToggleWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("simulated disk write failure"));
            }
            self.inner.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    impl std::io::Seek for ToggleWriter {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }

    /// A handle over a real WAV file with no audio device attached, so the
    /// file-ownership rules can be exercised without a microphone.
    fn handle_writing_to(path: &std::path::Path, padding_samples: u64) -> RecordingHandle {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec).unwrap();
        let (capture_error_tx, capture_error_rx) = tokio::sync::watch::channel(None);
        let writer = BatchWriter::start(writer, padding_samples, capture_error_tx.clone()).unwrap();
        RecordingHandle {
            stream: None,
            writer: Some(writer),
            recording: Arc::new(AtomicBool::new(true)),
            signal: SignalMonitor::default(),
            wav_path: Some(path.to_path_buf()),
            tail_capture: std::time::Duration::ZERO,
            preview_rx: None,
            dropped_preview_chunks: Arc::new(AtomicU64::new(0)),
            recorded_samples: Arc::new(AtomicU64::new(0)),
            capture_error_rx: Some(capture_error_rx),
            _capture_error_tx: capture_error_tx,
            system_audio_mute: None,
        }
    }

    /// Shutdown, a session restart, or a portal error can abandon a recording
    /// that was never stopped. Whatever the microphone captured must not stay
    /// on disk afterwards.
    #[test]
    fn dropping_an_unfinished_recording_removes_its_audio() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("voxkey_abandoned.wav");

        drop(handle_writing_to(&path, 0));

        assert!(!path.exists(), "abandoned recording left audio on disk");
    }

    /// The counterpart guarantee: a recording that completes normally hands a
    /// finalized file to the transcriber, which owns it from then on.
    #[tokio::test]
    async fn stopping_a_recording_hands_a_finalized_file_to_the_caller() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("voxkey_finished.wav");

        let audio_path = handle_writing_to(&path, 0).stop().await.unwrap();

        assert_eq!(audio_path, path);
        assert!(
            path.exists(),
            "stop() must leave the audio for transcription"
        );
        assert!(
            hound::WavReader::open(&path).is_ok(),
            "stop() must leave a readable WAV"
        );
    }

    #[tokio::test]
    async fn finalized_sample_count_excludes_synthetic_transcription_padding() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("voxkey_padded.wav");
        let handle = handle_writing_to(&path, 2);
        let writer = handle.writer.as_ref().unwrap().sender();
        assert!(
            queue_batch_samples(
                &writer,
                &handle._capture_error_tx,
                &handle.recorded_samples,
                u64::MAX,
                vec![1, 2, 3],
            )
            .is_some()
        );
        drop(writer);

        let (audio_path, captured_samples) = handle.stop_with_sample_count().await.unwrap();
        let reader = hound::WavReader::open(audio_path).unwrap();

        assert_eq!(captured_samples, 3);
        assert_eq!(reader.len(), 5);
    }

    #[tokio::test]
    async fn a_panicked_writer_worker_cannot_be_reported_as_finished() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        let writer = BatchWriter {
            sender: Some(sender),
            thread: Some(std::thread::spawn(|| -> Result<u64, String> {
                panic!("simulated WAV worker failure")
            })),
        };

        let error = writer.finish().await.unwrap_err();
        assert!(error.to_string().contains("panicked"), "{error}");
    }

    #[tokio::test]
    async fn a_capture_stream_error_cannot_be_reported_as_a_finished_recording() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("voxkey_truncated.wav");
        let handle = handle_writing_to(&path, 0);
        handle
            ._capture_error_tx
            .send_replace(Some("input device disconnected".to_string()));

        let error = handle
            .stop()
            .await
            .expect_err("a failed capture must not be transcribed");

        assert!(error.to_string().contains("input device disconnected"));
        assert!(!path.exists(), "a failed recording must be removed");
    }

    #[test]
    fn wav_sample_write_errors_are_retained_for_stop() {
        let fail = Arc::new(AtomicBool::new(false));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::new(
            ToggleWriter {
                inner: std::io::Cursor::new(Vec::new()),
                fail: fail.clone(),
            },
            spec,
        )
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);

        fail.store(true, Ordering::Relaxed);
        sender.send(Arc::from([1_i16, 2, 3])).unwrap();
        drop(sender);
        let error = run_wav_writer(writer, receiver, 0).unwrap_err();

        assert!(error.contains("simulated disk write failure"), "{error}");
    }

    #[test]
    fn slow_storage_never_blocks_the_realtime_audio_callback() {
        struct BlockingWriter {
            inner: std::io::Cursor<Vec<u8>>,
            block_next_write: Arc<AtomicBool>,
            entered: std::sync::mpsc::Sender<()>,
            gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }

        impl std::io::Write for BlockingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.block_next_write.swap(false, Ordering::Relaxed) {
                    let _ = self.entered.send(());
                    let (open, wake) = &*self.gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = wake.wait(open).unwrap();
                    }
                }
                self.inner.write(bytes)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.inner.flush()
            }
        }

        impl std::io::Seek for BlockingWriter {
            fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(position)
            }
        }

        let block_next_write = Arc::new(AtomicBool::new(false));
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::new(
            BlockingWriter {
                inner: std::io::Cursor::new(Vec::new()),
                block_next_write: block_next_write.clone(),
                entered: entered_tx,
                gate: gate.clone(),
            },
            spec,
        )
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(2);
        let worker = std::thread::spawn(move || run_wav_writer(writer, receiver, 0));
        let (errors, _observed) = tokio::sync::watch::channel(None);
        let samples = AtomicU64::new(0);

        block_next_write.store(true, Ordering::Relaxed);
        assert!(queue_batch_samples(&sender, &errors, &samples, 100, vec![1, 2]).is_some());
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("storage worker never entered its simulated slow write");
        let started = std::time::Instant::now();
        assert!(queue_batch_samples(&sender, &errors, &samples, 100, vec![3, 4]).is_some());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(20),
            "audio callback waited for storage for {:?}",
            started.elapsed()
        );

        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();
        drop(sender);
        assert_eq!(worker.join().unwrap().unwrap(), 4);
    }

    #[test]
    fn recorder_carries_configured_tail_capture_duration() {
        let config = AudioConfig {
            sample_rate: 16000,
            channels: 1,
            tail_capture_ms: 175,
            max_recording_seconds: 600,
            input_device: String::new(),
            mute_output_while_recording: false,
            behavior: crate::config::AudioBehaviorConfig::default(),
        };
        let recorder = Recorder::new(&config);
        assert_eq!(recorder.tail_capture, std::time::Duration::from_millis(175));
    }

    #[test]
    fn extreme_tail_capture_values_cannot_block_recording_stop_indefinitely() {
        let config = AudioConfig {
            sample_rate: 16000,
            channels: 1,
            tail_capture_ms: u32::MAX,
            max_recording_seconds: 600,
            input_device: String::new(),
            mute_output_while_recording: false,
            behavior: crate::config::AudioBehaviorConfig::default(),
        };

        let recorder = Recorder::new(&config);

        assert_eq!(
            recorder.tail_capture,
            std::time::Duration::from_millis(MAX_TAIL_CAPTURE_MS as u64)
        );
    }

    #[test]
    fn streaming_capture_errors_are_observable_without_blocking_the_callback() {
        let (errors, observed) = tokio::sync::watch::channel(None);

        report_streaming_capture_error(&errors, "input device disconnected".to_string());

        assert_eq!(
            observed.borrow().as_deref(),
            Some("input device disconnected")
        );
    }

    #[test]
    fn batch_capture_limit_is_observable_without_waiting_for_stop() {
        let (writer, _receiver) = std::sync::mpsc::sync_channel(1);
        let (errors, observed) = tokio::sync::watch::channel(None);
        let samples = AtomicU64::new(3);

        assert!(queue_batch_samples(&writer, &errors, &samples, 4, vec![4, 5]).is_none());

        assert!(
            observed
                .borrow()
                .as_deref()
                .is_some_and(|error| error.contains("limit"))
        );
        assert_eq!(samples.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn batch_file_budget_caps_extreme_audio_formats() {
        let samples = max_batch_samples(u32::MAX, u16::MAX, u32::MAX);
        assert_eq!(samples, 0);
    }

    #[test]
    fn a_full_streaming_buffer_is_reported_as_a_capture_error() {
        let (audio_tx, _audio_rx) = tokio::sync::mpsc::channel::<Arc<[i16]>>(1);
        audio_tx.try_send(Arc::from([1_i16, 2, 3])).unwrap();
        let (error_tx, error_rx) = tokio::sync::watch::channel(None);

        publish_streaming_samples(&audio_tx, &error_tx, vec![4, 5, 6]);

        let error = error_rx
            .borrow()
            .clone()
            .expect("dropped realtime audio must invalidate the transcript");
        assert!(error.contains("buffer"), "{error}");
    }

    #[test]
    fn exact_device_name_duplicates_are_removed_even_with_case_variants() {
        let names = normalize_input_device_names(vec![
            "Studio Mic".to_string(),
            "studio mic".to_string(),
            "Studio Mic".to_string(),
        ]);

        assert_eq!(names, ["Studio Mic", "studio mic"]);
    }

    #[test]
    fn a_closed_preview_consumer_is_not_reported_as_dropped_audio() {
        let (preview_tx, preview_rx) = tokio::sync::mpsc::channel(1);
        drop(preview_rx);
        let dropped = AtomicU64::new(0);

        publish_preview_chunk(Some(&preview_tx), &dropped, Arc::from([1_i16, 2, 3]));

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pactl_mute_output_is_parsed_strictly() {
        assert_eq!(parse_pactl_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_pactl_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_pactl_mute("yes"), None);
    }
}
