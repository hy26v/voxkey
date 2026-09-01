// ABOUTME: Computes lock-free microphone signal measurements inside the audio callback.
// ABOUTME: Provides one tested quality model for live guidance, diagnostics, and silence guards.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use voxkey_ipc::AudioSignalQuality;

/// Samples below this normalized amplitude count as background rather than
/// meaningful voice activity. The final guard also requires a sustained
/// duration, so a single click cannot masquerade as speech.
const ACTIVE_SAMPLE_AMPLITUDE: u32 = 256;
const CLIPPED_SAMPLE_AMPLITUDE: u32 = 32_600;
const MIN_MEANINGFUL_AUDIO_MS: u64 = 80;
const SILENT_MAX_PEAK: f64 = 0.01;
const QUIET_MAX_PEAK: f64 = 0.08;
const CLIPPED_SAMPLE_RATIO: f64 = 0.001;
const MIN_CLIPPED_AUDIO_MS: u64 = 3;
const CURRENT_VOICE_RMS: f64 = 0.008;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalSnapshot {
    pub latest_peak: f64,
    pub latest_rms: f64,
    pub max_peak: f64,
    pub average_rms: f64,
    pub total_samples: u64,
    pub active_samples: u64,
    pub clipped_samples: u64,
}

impl Default for SignalSnapshot {
    fn default() -> Self {
        Self {
            latest_peak: 0.0,
            latest_rms: 0.0,
            max_peak: 0.0,
            average_rms: 0.0,
            total_samples: 0,
            active_samples: 0,
            clipped_samples: 0,
        }
    }
}

impl SignalSnapshot {
    /// Whether enough non-background audio was captured to justify a batch
    /// transcription request. Short words still pass, while silence and
    /// isolated handling noise do not.
    pub fn has_meaningful_audio(self, sample_rate: u32, channels: u16) -> bool {
        if sample_rate == 0 || channels == 0 || self.max_peak < SILENT_MAX_PEAK {
            return false;
        }
        self.active_samples >= samples_for_millis(sample_rate, channels, MIN_MEANINGFUL_AUDIO_MS)
    }

    pub fn quality(self, sample_rate: u32, channels: u16) -> AudioSignalQuality {
        if sample_rate == 0 || channels == 0 || self.total_samples == 0 {
            return AudioSignalQuality::Silent;
        }
        let clipped_floor = samples_for_millis(sample_rate, channels, MIN_CLIPPED_AUDIO_MS);
        let clipped_ratio = if self.total_samples == 0 {
            0.0
        } else {
            self.clipped_samples as f64 / self.total_samples as f64
        };
        if self.clipped_samples >= clipped_floor || clipped_ratio >= CLIPPED_SAMPLE_RATIO {
            AudioSignalQuality::Clipping
        } else if !self.has_meaningful_audio(sample_rate, channels) {
            AudioSignalQuality::Silent
        } else if self.max_peak < QUIET_MAX_PEAK {
            AudioSignalQuality::Quiet
        } else {
            AudioSignalQuality::Good
        }
    }
}

/// Tracks a voice-aware automatic stop deadline without owning a timer. The
/// event loop feeds it monotonic elapsed time alongside the latest signal, so
/// the policy remains deterministic in tests and cannot outlive a recording.
#[derive(Debug, Clone)]
pub struct VoiceActivityStopwatch {
    silence_timeout: Option<std::time::Duration>,
    speech_started: bool,
    quiet_since: Option<std::time::Duration>,
}

impl VoiceActivityStopwatch {
    pub fn new(silence_timeout_ms: u32) -> Self {
        Self {
            silence_timeout: (silence_timeout_ms != 0)
                .then(|| std::time::Duration::from_millis(u64::from(silence_timeout_ms))),
            speech_started: false,
            quiet_since: None,
        }
    }

    /// Return true once meaningful speech has been followed by the configured
    /// quiet interval. Initial room silence never ends a recording.
    pub fn observe(
        &mut self,
        signal: SignalSnapshot,
        sample_rate: u32,
        channels: u16,
        elapsed: std::time::Duration,
    ) -> bool {
        let Some(timeout) = self.silence_timeout else {
            return false;
        };
        let currently_voiced =
            signal.latest_rms >= CURRENT_VOICE_RMS || signal.latest_peak >= SILENT_MAX_PEAK * 2.0;

        if !self.speech_started {
            if !signal.has_meaningful_audio(sample_rate, channels) {
                return false;
            }
            self.speech_started = true;
        }

        if currently_voiced {
            self.quiet_since = None;
            return false;
        }

        let quiet_since = self.quiet_since.get_or_insert(elapsed);
        elapsed.saturating_sub(*quiet_since) >= timeout
    }
}

#[derive(Default)]
struct SignalMetrics {
    latest_peak: AtomicU32,
    latest_rms_millionths: AtomicU32,
    max_peak: AtomicU32,
    squared_samples: AtomicU64,
    total_samples: AtomicU64,
    active_samples: AtomicU64,
    clipped_samples: AtomicU64,
}

/// Cloneable signal monitor shared by a CPAL callback and the async control
/// loop. `observe` never locks or allocates, which keeps it safe for real-time
/// capture. Readers receive a coherent-enough monotonic summary without ever
/// blocking the callback.
#[derive(Clone, Default)]
pub struct SignalMonitor {
    metrics: Arc<SignalMetrics>,
}

impl SignalMonitor {
    pub fn observe(&self, samples: &[i16]) {
        let mut peak = 0_u32;
        let mut squared = 0_u64;
        let mut active = 0_u64;
        let mut clipped = 0_u64;

        for sample in samples {
            let amplitude = u32::from(sample.unsigned_abs());
            peak = peak.max(amplitude);
            squared = squared.saturating_add(u64::from(amplitude) * u64::from(amplitude));
            active += u64::from(amplitude >= ACTIVE_SAMPLE_AMPLITUDE);
            clipped += u64::from(amplitude >= CLIPPED_SAMPLE_AMPLITUDE);
        }

        let rms = if samples.is_empty() {
            0.0
        } else {
            (squared as f64 / samples.len() as f64).sqrt() / 32_768.0
        };
        self.metrics.latest_peak.store(peak, Ordering::Relaxed);
        self.metrics.latest_rms_millionths.store(
            (rms.clamp(0.0, 1.0) * 1_000_000.0).round() as u32,
            Ordering::Relaxed,
        );
        self.metrics.max_peak.fetch_max(peak, Ordering::Relaxed);
        saturating_fetch_add(&self.metrics.squared_samples, squared);
        saturating_fetch_add(&self.metrics.total_samples, samples.len() as u64);
        saturating_fetch_add(&self.metrics.active_samples, active);
        saturating_fetch_add(&self.metrics.clipped_samples, clipped);
    }

    pub fn snapshot(&self) -> SignalSnapshot {
        let total_samples = self.metrics.total_samples.load(Ordering::Relaxed);
        let squared_samples = self.metrics.squared_samples.load(Ordering::Relaxed);
        SignalSnapshot {
            latest_peak: normalized_amplitude(self.metrics.latest_peak.load(Ordering::Relaxed)),
            latest_rms: f64::from(self.metrics.latest_rms_millionths.load(Ordering::Relaxed))
                / 1_000_000.0,
            max_peak: normalized_amplitude(self.metrics.max_peak.load(Ordering::Relaxed)),
            average_rms: if total_samples == 0 {
                0.0
            } else {
                (squared_samples as f64 / total_samples as f64).sqrt() / 32_768.0
            },
            total_samples,
            active_samples: self.metrics.active_samples.load(Ordering::Relaxed),
            clipped_samples: self.metrics.clipped_samples.load(Ordering::Relaxed),
        }
    }
}

fn normalized_amplitude(amplitude: u32) -> f64 {
    f64::from(amplitude.min(32_768)) / 32_768.0
}

fn samples_for_millis(sample_rate: u32, channels: u16, millis: u64) -> u64 {
    u64::from(sample_rate)
        .saturating_mul(u64::from(channels))
        .saturating_mul(millis)
        / 1_000
}

fn saturating_fetch_add(target: &AtomicU64, amount: u64) {
    let _ = target.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_reports_peak_rms_and_monotonic_totals() {
        let monitor = SignalMonitor::default();
        monitor.observe(&[0, 16_384, -16_384, 0]);
        let first = monitor.snapshot();
        assert!((first.latest_peak - 0.5).abs() < f64::EPSILON);
        assert!((first.latest_rms - 0.353_553).abs() < 0.000_002);
        assert!((first.average_rms - 0.353_553).abs() < 0.000_002);
        assert_eq!(first.total_samples, 4);
        assert_eq!(first.active_samples, 2);

        monitor.observe(&[32_767, 0]);
        let second = monitor.snapshot();
        assert_eq!(second.total_samples, 6);
        assert!(second.max_peak > 0.99);
        assert!(second.average_rms > first.average_rms);
    }

    #[test]
    fn empty_chunks_reset_live_values_without_erasing_the_summary() {
        let monitor = SignalMonitor::default();
        monitor.observe(&[8_192]);
        monitor.observe(&[]);
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.latest_peak, 0.0);
        assert_eq!(snapshot.latest_rms, 0.0);
        assert_eq!(snapshot.total_samples, 1);
        assert_eq!(snapshot.max_peak, 0.25);
    }

    #[test]
    fn meaningful_audio_requires_sustained_activity() {
        let short_click = SignalSnapshot {
            max_peak: 1.0,
            active_samples: 100,
            total_samples: 16_000,
            ..Default::default()
        };
        assert!(!short_click.has_meaningful_audio(16_000, 1));

        let short_word = SignalSnapshot {
            max_peak: 0.2,
            active_samples: 1_280,
            total_samples: 4_000,
            ..Default::default()
        };
        assert!(short_word.has_meaningful_audio(16_000, 1));
    }

    #[test]
    fn quality_distinguishes_silent_quiet_good_and_clipping() {
        let snapshot = |max_peak: f64, clipped_samples: u64| SignalSnapshot {
            max_peak,
            total_samples: 16_000,
            active_samples: 2_000,
            clipped_samples,
            ..Default::default()
        };
        assert_eq!(
            SignalSnapshot::default().quality(16_000, 1),
            AudioSignalQuality::Silent
        );
        assert_eq!(
            SignalSnapshot::default().quality(0, 0),
            AudioSignalQuality::Silent
        );
        assert_eq!(
            snapshot(0.05, 0).quality(16_000, 1),
            AudioSignalQuality::Quiet
        );
        assert_eq!(
            snapshot(0.5, 0).quality(16_000, 1),
            AudioSignalQuality::Good
        );
        assert_eq!(
            snapshot(1.0, 48).quality(16_000, 1),
            AudioSignalQuality::Clipping
        );
    }

    #[test]
    fn clipping_ratio_catches_short_saturated_tests() {
        let snapshot = SignalSnapshot {
            max_peak: 1.0,
            total_samples: 2_000,
            active_samples: 1_000,
            clipped_samples: 3,
            ..Default::default()
        };
        assert_eq!(snapshot.quality(48_000, 2), AudioSignalQuality::Clipping);
    }

    #[test]
    fn automatic_stop_ignores_silence_until_speech_has_started() {
        let mut stopwatch = VoiceActivityStopwatch::new(1_500);
        for elapsed_ms in [0, 1_500, 15_000] {
            assert!(!stopwatch.observe(
                SignalSnapshot::default(),
                16_000,
                1,
                std::time::Duration::from_millis(elapsed_ms),
            ));
        }
    }

    #[test]
    fn automatic_stop_restarts_its_deadline_when_speech_resumes() {
        let mut stopwatch = VoiceActivityStopwatch::new(1_500);
        let speech = SignalSnapshot {
            latest_peak: 0.2,
            latest_rms: 0.04,
            max_peak: 0.2,
            total_samples: 2_000,
            active_samples: 1_500,
            ..Default::default()
        };
        let quiet = SignalSnapshot {
            max_peak: 0.2,
            total_samples: 20_000,
            active_samples: 1_500,
            ..Default::default()
        };

        assert!(!stopwatch.observe(speech, 16_000, 1, std::time::Duration::from_millis(100),));
        assert!(!stopwatch.observe(quiet, 16_000, 1, std::time::Duration::from_millis(500),));
        assert!(!stopwatch.observe(speech, 16_000, 1, std::time::Duration::from_millis(1_000),));
        assert!(!stopwatch.observe(quiet, 16_000, 1, std::time::Duration::from_millis(1_200),));
        assert!(stopwatch.observe(quiet, 16_000, 1, std::time::Duration::from_millis(2_700),));
    }

    #[test]
    fn zero_timeout_disables_automatic_stop() {
        let mut stopwatch = VoiceActivityStopwatch::new(0);
        let speech_then_quiet = SignalSnapshot {
            max_peak: 0.2,
            total_samples: 32_000,
            active_samples: 2_000,
            ..Default::default()
        };
        assert!(!stopwatch.observe(
            speech_then_quiet,
            16_000,
            1,
            std::time::Duration::from_secs(60),
        ));
    }
}
