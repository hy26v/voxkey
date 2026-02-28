// ABOUTME: Records audio from the default input device to a temporary WAV file.
// ABOUTME: Uses cpal for audio capture and hound for WAV encoding at 16kHz mono 16-bit.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::AudioConfig;

/// Records audio from the default input device.
pub struct Recorder {
    sample_rate: u32,
    channels: u16,
}

impl Recorder {
    pub fn new(config: &AudioConfig) -> Self {
        Self {
            sample_rate: config.sample_rate,
            channels: config.channels,
        }
    }

    /// Start streaming audio capture. Returns a handle with a channel receiver
    /// that delivers raw PCM i16 chunks for real-time processing.
    pub fn start_streaming(&self) -> Result<StreamingRecordingHandle, Box<dyn std::error::Error + Send + Sync>> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("No default input device available")?;

        tracing::info!("Streaming from: {}", device.name().unwrap_or_default());

        let desired_config = cpal::StreamConfig {
            channels: self.channels,
            sample_rate: cpal::SampleRate(self.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<i16>>(64);

        let recording = Arc::new(AtomicBool::new(true));
        let recording_clone = recording.clone();

        let stream = device.build_input_stream(
            &desired_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                if !recording_clone.load(Ordering::Relaxed) {
                    return;
                }
                // Drop chunks if receiver is behind — lossy is better than blocking audio
                let _ = tx.try_send(data.to_vec());
            },
            move |err| {
                tracing::error!("Audio input error: {err}");
            },
            None,
        )?;

        stream.play()?;
        tracing::info!("Streaming recording started");

        Ok(StreamingRecordingHandle {
            stream: Some(stream),
            recording,
            rx: Some(rx),
        })
    }

    /// Start recording. Returns a handle that can be used to stop recording.
    pub fn start(&self) -> Result<RecordingHandle, Box<dyn std::error::Error + Send + Sync>> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("No default input device available")?;

        tracing::info!("Recording from: {}", device.name().unwrap_or_default());

        let desired_config = cpal::StreamConfig {
            channels: self.channels,
            sample_rate: cpal::SampleRate(self.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        // Create a temp file that persists until the transcriber cleans it up.
        // keep() disables auto-deletion on drop and returns (File, PathBuf).
        let (_file, wav_path) = tempfile::Builder::new()
            .prefix("voxkey_")
            .suffix(".wav")
            .tempfile()?
            .keep()?;

        let spec = hound::WavSpec {
            channels: self.channels,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&wav_path, spec)?;
        let writer = Arc::new(Mutex::new(Some(writer)));

        let recording = Arc::new(AtomicBool::new(true));
        let recording_clone = recording.clone();
        let writer_clone = writer.clone();

        let stream = device.build_input_stream(
            &desired_config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                if !recording_clone.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut guard) = writer_clone.lock() {
                    if let Some(ref mut w) = *guard {
                        for &sample in data {
                            let _ = w.write_sample(sample);
                        }
                    }
                }
            },
            move |err| {
                tracing::error!("Audio input error: {err}");
            },
            None,
        )?;

        stream.play()?;
        tracing::info!("Recording started");

        Ok(RecordingHandle {
            stream: Some(stream),
            writer,
            recording,
            wav_path,
        })
    }
}

/// Handle to a streaming audio capture. Provides a channel of raw PCM chunks.
pub struct StreamingRecordingHandle {
    stream: Option<cpal::Stream>,
    recording: Arc<AtomicBool>,
    rx: Option<tokio::sync::mpsc::Receiver<Vec<i16>>>,
}

impl StreamingRecordingHandle {
    /// Take the audio chunk receiver. Can only be called once.
    pub fn take_rx(&mut self) -> Option<tokio::sync::mpsc::Receiver<Vec<i16>>> {
        self.rx.take()
    }

    /// Stop the audio capture stream.
    pub fn stop(&mut self) {
        self.recording.store(false, Ordering::Relaxed);
        drop(self.stream.take());
        tracing::info!("Streaming recording stopped");
    }
}

/// Handle to an in-progress recording. Call `stop()` to finalize the WAV file.
pub struct RecordingHandle {
    stream: Option<cpal::Stream>,
    writer: Arc<Mutex<Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>>>,
    recording: Arc<AtomicBool>,
    wav_path: PathBuf,
}

impl RecordingHandle {
    /// Stop recording and finalize the WAV file. Returns the path to the WAV file.
    /// Captures a short tail of audio before stopping to avoid cutting off the last words.
    pub async fn stop(mut self) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        // Keep capturing briefly so in-flight audio buffers are flushed to the WAV
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        self.recording.store(false, Ordering::Relaxed);

        // Drop the stream to stop capturing
        drop(self.stream.take());

        // Finalize the WAV file
        if let Ok(mut guard) = self.writer.lock() {
            if let Some(writer) = guard.take() {
                writer.finalize()?;
            }
        }

        tracing::info!("Recording stopped, saved to: {}", self.wav_path.display());
        Ok(self.wav_path)
    }
}
