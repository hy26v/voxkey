// ABOUTME: Splits a PCM stream into utterances using energy-based silence detection.
// ABOUTME: Bounds are reported in interleaved frames and padded at both ends of each utterance.

use std::collections::VecDeque;
use std::ops::Range;

/// Lowest useful energy threshold. The active threshold normally sits above
/// this and follows the microphone's measured noise floor.
const MIN_RMS_THRESHOLD: f64 = 0.006;
const NOISE_MULTIPLIER: f64 = 1.8;
const NOISE_MARGIN: f64 = 0.003;
const BOOTSTRAP_WINDOWS: usize = 5;
const NOISE_HISTORY_WINDOWS: usize = 30;
const MAX_STATIONARY_NOISE: f64 = 0.08;
/// Length of one RMS analysis window.
const WINDOW_MILLIS: u64 = 100;
/// Continuous silence after speech that closes an utterance. The boundary is
/// placed at the end of the last voiced window, not at the end of the silence.
const MIN_SILENCE_TO_CLOSE_MILLIS: u64 = 1200;
/// Voiced audio a closed region needs to stand on its own as an utterance.
const MIN_SPEECH_TO_COMMIT_MILLIS: u64 = 250;
/// Padding applied to both ends of each reported segment. The start pad keeps
/// soft onsets that sit below the silence threshold from being clipped before
/// decoding; regions are always separated by more silence than both pads
/// together, so neighbours never overlap.
const BOUNDARY_PAD_MILLIS: u64 = 250;

/// Incremental silence-based utterance splitter fed with interleaved i16 PCM.
pub struct OnlineSplitter {
    channels: usize,
    window_frames: u64,
    window_samples: usize,
    min_speech_frames: u64,
    pad_frames: u64,
    pending: Vec<i16>,
    total_frames: u64,
    analyzed_frames: u64,
    last_had_speech: bool,
    vad: AdaptiveEnergyVad,
    tracker: RegionTracker,
    /// Closed region with too little speech to stand alone, waiting to be
    /// merged into the next region instead of being dropped.
    carried: Option<Region>,
    /// Padded end of the last emitted range; the next start pad stops here.
    last_emitted_end: u64,
}

/// Number of frames spanned by `millis` at `sample_rate`.
fn frames_for(sample_rate: u32, millis: u64) -> u64 {
    (sample_rate as u64 * millis) / 1000
}

/// RMS of interleaved samples normalized to full scale. Empty input is silent.
fn window_rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_squares: f64 = samples
        .iter()
        .map(|&sample| {
            let normalized = sample as f64 / 32768.0;
            normalized * normalized
        })
        .sum();
    (sum_squares / samples.len() as f64).sqrt()
}

struct ClassifiedWindow {
    start: u64,
    end: u64,
    speech: bool,
    reset_false_positive: bool,
}

struct PendingWindow {
    start: u64,
    end: u64,
    rms: f64,
}

/// Energy VAD with a running microphone noise-floor estimate. The first half
/// second is classified together, so a noisy mic is calibrated before it can
/// become a fake utterance while immediate speech is still recovered from the
/// buffered windows.
struct AdaptiveEnergyVad {
    noise_floor: Option<f64>,
    bootstrap: Vec<PendingWindow>,
    history: VecDeque<f64>,
}

impl AdaptiveEnergyVad {
    fn new() -> Self {
        Self {
            noise_floor: None,
            bootstrap: Vec::with_capacity(BOOTSTRAP_WINDOWS),
            history: VecDeque::with_capacity(NOISE_HISTORY_WINDOWS),
        }
    }

    fn push(
        &mut self,
        start: u64,
        end: u64,
        rms: f64,
        speech_active: bool,
    ) -> Vec<ClassifiedWindow> {
        if self.noise_floor.is_none() {
            self.bootstrap.push(PendingWindow { start, end, rms });
            if self.bootstrap.len() < BOOTSTRAP_WINDOWS {
                return Vec::new();
            }
            let mut energies = self
                .bootstrap
                .iter()
                .map(|window| window.rms)
                .collect::<Vec<_>>();
            energies.sort_by(f64::total_cmp);
            let lower_quintile = energies[(energies.len() - 1) / 5];
            let floor = lower_quintile.clamp(0.0005, 0.015);
            self.noise_floor = Some(floor);
            self.history.extend(energies);
            let threshold = self.threshold(false);
            return self
                .bootstrap
                .drain(..)
                .map(|window| ClassifiedWindow {
                    start: window.start,
                    end: window.end,
                    speech: window.rms >= threshold,
                    reset_false_positive: false,
                })
                .collect();
        }

        self.history.push_back(rms);
        if self.history.len() > NOISE_HISTORY_WINDOWS {
            self.history.pop_front();
        }

        let mut reset_false_positive = false;
        if self.history.len() >= BOOTSTRAP_WINDOWS {
            let recent = self
                .history
                .iter()
                .rev()
                .take(BOOTSTRAP_WINDOWS)
                .copied()
                .collect::<Vec<_>>();
            let low = recent.iter().copied().fold(f64::INFINITY, f64::min);
            let high = recent.iter().copied().fold(0.0_f64, f64::max);
            let stationary = low > 0.0 && high / low < 1.3;
            let current_floor = self.noise_floor.unwrap_or(MIN_RMS_THRESHOLD);
            if stationary && low > current_floor * 1.5 && high <= MAX_STATIONARY_NOISE {
                self.noise_floor = Some(low);
                reset_false_positive = speech_active;
            }
        }

        let threshold = self.threshold(speech_active && !reset_false_positive);
        let speech = !reset_false_positive && rms >= threshold;
        if !speech {
            let floor = self.noise_floor.unwrap_or(rms);
            self.noise_floor = Some((floor * 0.9 + rms * 0.1).clamp(0.0005, 0.2));
        }
        vec![ClassifiedWindow {
            start,
            end,
            speech,
            reset_false_positive,
        }]
    }

    fn threshold(&self, speech_active: bool) -> f64 {
        let floor = self.noise_floor.unwrap_or(MIN_RMS_THRESHOLD);
        let threshold = (floor * NOISE_MULTIPLIER + NOISE_MARGIN).max(MIN_RMS_THRESHOLD);
        if speech_active {
            threshold * 0.75
        } else {
            threshold
        }
    }
}

/// Shared per-window accounting for one utterance region under construction.
struct RegionTracker {
    window_frames: u64,
    min_silence_frames: u64,
    in_region: bool,
    region_start: u64,
    region_speech_frames: u64,
    last_speech_end: u64,
    silence_frames: u64,
}

impl RegionTracker {
    fn new(window_frames: u64, min_silence_frames: u64) -> Self {
        Self {
            window_frames,
            min_silence_frames,
            in_region: false,
            region_start: 0,
            region_speech_frames: 0,
            last_speech_end: 0,
            silence_frames: 0,
        }
    }

    /// Fold one classified window into the state. Returns a closed region when
    /// enough trailing silence has accumulated, with the end at the last speech.
    fn push_window(&mut self, start: u64, end: u64, speech: bool) -> Option<Region> {
        if !self.in_region {
            if speech {
                self.in_region = true;
                self.region_start = start;
                self.region_speech_frames = self.window_frames;
                self.last_speech_end = end;
                self.silence_frames = 0;
            }
            return None;
        }

        if speech {
            self.region_speech_frames += self.window_frames;
            self.last_speech_end = end;
            self.silence_frames = 0;
            return None;
        }

        self.silence_frames += self.window_frames;
        if self.silence_frames < self.min_silence_frames {
            return None;
        }
        Some(self.take_region())
    }

    /// Extract the region currently under construction and reset to idle.
    fn take_region(&mut self) -> Region {
        let region = Region {
            start: self.region_start,
            end: self.last_speech_end,
            speech_frames: self.region_speech_frames,
        };
        self.in_region = false;
        self.region_speech_frames = 0;
        self.silence_frames = 0;
        region
    }

    fn discard_region(&mut self) {
        self.in_region = false;
        self.region_speech_frames = 0;
        self.silence_frames = 0;
    }
}

/// A voiced run located in the stream, before end padding is applied.
struct Region {
    start: u64,
    end: u64,
    speech_frames: u64,
}

impl OnlineSplitter {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1) as usize;
        let window_frames = frames_for(sample_rate, WINDOW_MILLIS);
        let min_silence_frames = frames_for(sample_rate, MIN_SILENCE_TO_CLOSE_MILLIS);
        Self {
            channels,
            window_frames,
            window_samples: window_frames as usize * channels,
            min_speech_frames: frames_for(sample_rate, MIN_SPEECH_TO_COMMIT_MILLIS),
            pad_frames: frames_for(sample_rate, BOUNDARY_PAD_MILLIS),
            pending: Vec::new(),
            total_frames: 0,
            analyzed_frames: 0,
            last_had_speech: false,
            vad: AdaptiveEnergyVad::new(),
            tracker: RegionTracker::new(window_frames, min_silence_frames),
            carried: None,
            last_emitted_end: 0,
        }
    }

    /// Feed captured audio and return the utterances this push closed.
    pub fn push(&mut self, samples: &[i16]) -> Vec<Range<u64>> {
        let mut closed = Vec::new();
        if samples.is_empty() {
            return closed;
        }
        self.total_frames += (samples.len() / self.channels) as u64;
        self.pending.extend_from_slice(samples);
        self.last_had_speech = false;

        while self.pending.len() >= self.window_samples {
            let window: Vec<i16> = self.pending.drain(..self.window_samples).collect();
            let start = self.analyzed_frames;
            let end = start + self.window_frames;
            self.analyzed_frames = end;
            let classified = self
                .vad
                .push(start, end, window_rms(&window), self.tracker.in_region);
            for decision in classified {
                if decision.reset_false_positive {
                    self.tracker.discard_region();
                    self.last_had_speech = false;
                }
                self.last_had_speech |= decision.speech;

                if let Some(region) =
                    self.tracker
                        .push_window(decision.start, decision.end, decision.speech)
                {
                    let region = self.absorb_carried(region);
                    if region.speech_frames >= self.min_speech_frames {
                        let padded_start = region
                            .start
                            .saturating_sub(self.pad_frames)
                            .max(self.last_emitted_end);
                        let padded_end = (region.end + self.pad_frames).min(self.total_frames);
                        if padded_end > padded_start {
                            self.last_emitted_end = padded_end;
                            closed.push(padded_start..padded_end);
                        }
                    } else {
                        self.carried = Some(region);
                    }
                }
            }
        }

        closed
    }

    /// Merge a carried short region into the region that follows it, so the
    /// short speech is decoded as part of that utterance instead of being
    /// dropped from the transcription.
    fn absorb_carried(&mut self, region: Region) -> Region {
        match self.carried.take() {
            Some(short) => Region {
                start: short.start,
                end: region.end,
                speech_frames: short.speech_frames + region.speech_frames,
            },
            None => region,
        }
    }

    /// Total frames received so far, including audio not yet analyzed.
    #[cfg(test)]
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Whether the most recent non-empty push contained a voiced window.
    pub fn last_push_had_speech(&self) -> bool {
        self.last_had_speech
    }

    /// Start of the utterance currently being accumulated, pulled back by the
    /// start padding, or None while nothing is pending. A carried short region
    /// will join the next utterance, so it anchors the start while it waits.
    pub fn open_region_padded_start(&self) -> Option<u64> {
        let start = if self.tracker.in_region {
            let current = self.tracker.region_start;
            self.carried
                .as_ref()
                .map_or(current, |carried| carried.start.min(current))
        } else {
            self.carried.as_ref()?.start
        };
        Some(start.saturating_sub(self.pad_frames))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    /// Constant-amplitude samples whose RMS is the amplitude itself once
    /// normalized, so the energy is exactly known.
    fn tone(frames: u64, channels: u16, amplitude: i16) -> Vec<i16> {
        vec![amplitude; (frames * channels as u64) as usize]
    }

    fn silence(frames: u64, channels: u16) -> Vec<i16> {
        tone(frames, channels, 0)
    }

    fn speech(frames: u64, channels: u16) -> Vec<i16> {
        tone(frames, channels, 10_000)
    }

    fn frames(millis: u64) -> u64 {
        (RATE as u64 * millis) / 1000
    }

    #[test]
    fn online_closes_an_utterance_at_the_speech_end() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        let mut samples = speech(frames(600), 1);
        samples.extend(silence(frames(1300), 1));

        let closed = splitter.push(&samples);

        // Boundary sits at the last voiced frame plus the end padding, not at
        // the end of the trailing silence.
        assert_eq!(closed, vec![0..frames(600) + frames(250)]);
        assert_eq!(splitter.total_frames(), frames(1900));
    }

    #[test]
    fn online_does_not_close_while_speech_continues() {
        let mut splitter = OnlineSplitter::new(RATE, 1);

        let closed = splitter.push(&speech(frames(2000), 1));

        assert!(closed.is_empty());
        assert_eq!(splitter.total_frames(), frames(2000));
    }

    #[test]
    fn online_keeps_a_region_open_across_gaps_below_the_silence_limit() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        let mut samples = speech(frames(600), 1);
        samples.extend(silence(frames(500), 1));
        samples.extend(speech(frames(600), 1));

        let closed = splitter.push(&samples);

        assert!(closed.is_empty(), "a 0.5s gap must not close the region");
    }

    #[test]
    fn online_keeps_a_sub_250ms_blip_pending_for_the_next_utterance() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        let mut samples = speech(frames(200), 1);
        samples.extend(silence(frames(1300), 1));

        let closed = splitter.push(&samples);

        assert!(
            closed.is_empty(),
            "a 0.2s blip is not a standalone utterance"
        );
        assert_eq!(
            splitter.open_region_padded_start(),
            Some(0),
            "the blip is carried, not dropped, and anchors the pending start"
        );
    }

    #[test]
    fn online_merges_a_short_blip_into_the_following_utterance() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        let mut samples = speech(frames(600), 1);
        samples.extend(silence(frames(1300), 1));
        samples.extend(speech(frames(200), 1));
        samples.extend(silence(frames(1300), 1));
        samples.extend(speech(frames(600), 1));
        samples.extend(silence(frames(1300), 1));

        let closed = splitter.push(&samples);

        // The blip joins the utterance after it instead of vanishing, and
        // both ranges keep their start padding within the surrounding silence.
        assert_eq!(
            closed,
            vec![0..frames(600) + frames(250), frames(1650)..frames(4250),]
        );
    }

    #[test]
    fn open_region_start_survives_a_carried_blip_into_the_next_region() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        let mut samples = speech(frames(600), 1);
        samples.extend(silence(frames(1300), 1));
        samples.extend(speech(frames(200), 1));
        samples.extend(silence(frames(1300), 1));
        splitter.push(&samples);
        splitter.push(&speech(frames(100), 1));

        assert_eq!(
            splitter.open_region_padded_start(),
            Some(frames(1650)),
            "the carried blip, not the new region, anchors the pending start"
        );
    }

    #[test]
    fn silence_only_audio_yields_no_utterances() {
        let samples = silence(frames(3000), 1);

        let mut splitter = OnlineSplitter::new(RATE, 1);
        assert!(splitter.push(&samples).is_empty());
    }

    #[test]
    fn an_elevated_stationary_noise_floor_does_not_become_speech() {
        // 1000/32768 = 0.0305, well above the old fixed 0.02 threshold.
        let noise = tone(frames(3000), 1, 1000);
        let mut splitter = OnlineSplitter::new(RATE, 1);
        assert!(splitter.push(&noise).is_empty());
        assert!(!splitter.last_push_had_speech());
    }

    #[test]
    fn speech_is_detected_over_the_degrader_noise_floor() {
        // Match scripts/degrade_fixtures.sh's default 0.012 noise amplitude,
        // then prove ordinary speech still clears the learned threshold.
        let mut samples = tone(frames(600), 1, 393);
        samples.extend(speech(frames(600), 1));
        samples.extend(tone(frames(1300), 1, 393));
        let mut splitter = OnlineSplitter::new(RATE, 1);
        assert_eq!(splitter.push(&samples).len(), 1);
    }

    #[test]
    fn online_reports_speech_presence_per_push() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        splitter.push(&silence(frames(300), 1));
        assert!(!splitter.last_push_had_speech());

        splitter.push(&speech(frames(300), 1));
        assert!(splitter.last_push_had_speech());

        splitter.push(&silence(frames(300), 1));
        assert!(!splitter.last_push_had_speech());
    }

    #[test]
    fn online_exposes_the_open_region_start_with_pad() {
        let mut splitter = OnlineSplitter::new(RATE, 1);
        assert_eq!(splitter.open_region_padded_start(), None);

        splitter.push(&speech(frames(600), 1));
        assert_eq!(splitter.open_region_padded_start(), Some(0));

        splitter.push(&silence(frames(1300), 1));
        assert_eq!(
            splitter.open_region_padded_start(),
            None,
            "the region closed after the silence"
        );
    }

    #[test]
    fn stereo_input_is_counted_in_frames() {
        let mut splitter = OnlineSplitter::new(RATE, 2);
        let mut samples = speech(frames(600), 2);
        samples.extend(silence(frames(1300), 2));

        let closed = splitter.push(&samples);

        assert_eq!(closed, vec![0..frames(600) + frames(250)]);
        assert_eq!(splitter.total_frames(), frames(1900));
    }
}
