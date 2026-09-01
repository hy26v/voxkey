// ABOUTME: Turns a growing batch recording into committed utterances plus a live tail preview.
// ABOUTME: Committed segments are append-only so earlier words never jump or vanish mid-recording.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::agreement::{TimedWord, WordAgreementEngine, estimate_word_timings};
use crate::config::PreviewStrategy;
use crate::dbus::{DaemonInterface, SharedState};
use crate::segmentation::OnlineSplitter;
use crate::transcriber::{self, Transcriber};

/// Minimum tail audio before a preview is worth requesting. Previewing a sliver
/// of audio wastes a provider round trip and yields an unstable hypothesis.
const PREVIEW_MIN_AUDIO: Duration = Duration::from_millis(1200);

/// Context retained before the first unconfirmed word. It protects soft word
/// onsets and gives the recognizer enough acoustic history without decoding
/// the confirmed beginning of a long recording again.
const AGREEMENT_LOOKBACK: Duration = Duration::from_millis(1500);

/// Text-only providers distribute words across a snapshot because they expose
/// no trustworthy timestamps. More left context keeps that estimated seek
/// from beginning mid-utterance and corrupting the retained hypothesis.
const ESTIMATED_AGREEMENT_LOOKBACK: Duration = Duration::from_secs(5);

/// Models use the artificial quiet tail to finish the last word and emit the
/// punctuation that would otherwise be withheld at a mid-utterance snapshot.
pub(crate) const TRAILING_SILENCE: Duration = Duration::from_secs(1);

/// Floor on how long a single preview request may run. A hypothesis that
/// arrives many intervals late is worthless, and a wedged provider would
/// otherwise block every later preview and delay the final transcription.
const MIN_PREVIEW_JOB_TIMEOUT: Duration = Duration::from_secs(15);

/// Do not let a slow live-preview pass hold the Stop control request hostage.
/// Fast local decodes still get a chance to become the reusable final, while a
/// stalled pass is cancelled so the independently cancellable final decode can
/// start and the daemon remains responsive.
const FINAL_PREVIEW_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Consecutive provider errors tolerated before previewing and committing give
/// up for the remainder of a recording. The tail buffer survives so the final
/// transcription can still decode the whole recording.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Once a sentence prefix is confirmed, a single wild decode must not erase
/// a large portion of the still-visible hypothesis. Small corrections remain
/// allowed in the two intentionally unconfirmed sentences.
const MAX_LATE_HYPOTHESIS_DROPS: usize = 2;

type PreviewJobResult = Result<Option<DecodedPreview>, String>;

/// Audio captured since the last committed utterance, shared with the writer
/// rather than copied. Chunks arrive from the recorder already boxed.
type AudioSnapshot = Vec<Arc<[i16]>>;

/// Immutable audio plus its absolute frame range in the recording.
struct RangedSnapshot {
    audio: AudioSnapshot,
    frames: Range<u64>,
}

#[derive(Debug, PartialEq)]
struct DecodedPreview {
    text: String,
    words: Vec<TimedWord>,
}

/// The job currently occupying the single in-flight slot. Preview jobs carry
/// how many samples their snapshot covers so a complete decode can later be
/// reused as the final transcript.
enum InFlightKind {
    Commit(Range<u64>),
    Preview {
        decoded_from_frame: u64,
        covered_through_frame: u64,
    },
}

/// Owns the background preview supervisor. Dropping it cancels the supervisor;
/// `stop()` additionally tells an in-flight provider request to give up before
/// it reaches work that cannot be interrupted.
pub struct PreviewHandle {
    task: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<SegmentedPreview>>,
    whole: bool,
}

pub struct PreviewSession {
    pub sample_rate: u32,
    pub channels: u16,
    pub transcriber: Arc<Transcriber>,
    pub replacement_rules: Vec<voxkey_ipc::WordReplacement>,
    pub shared: SharedState,
    pub connection: zbus::Connection,
    pub generation: u64,
    pub interval: Duration,
    pub max_audio: Duration,
    pub strategy: PreviewStrategy,
}

impl PreviewHandle {
    /// Stop previewing. Signals cancellation first so a provider request that
    /// has not yet reached its uninterruptible section exits on its own, then
    /// waits for the supervisor to unwind.
    pub async fn stop(mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.abort();
            // A panic here means previews stopped silently earlier in the
            // recording. Surface it rather than losing it to an ignored join.
            if let Err(error) = task.await
                && error.is_panic()
            {
                tracing::error!("Preview supervisor panicked: {error}");
            }
        }
    }

    /// Let the supervisor drain any in-flight decode instead of cancelling it,
    /// then hand back the newest successful decode so the final transcription
    /// can reuse it. Only whole-stream previews decode the full recording, so
    /// only they can stand in for the final transcript. Call this after the
    /// recording has stopped so the audio stream is closed and the supervisor
    /// can finish on its own.
    pub async fn finish(mut self) -> Option<(u64, String)> {
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(FINAL_PREVIEW_DRAIN_TIMEOUT, &mut task).await {
                Ok(Err(error)) if error.is_panic() => {
                    tracing::error!("Preview supervisor panicked: {error}");
                }
                Ok(_) => {}
                Err(_) => {
                    self.cancelled.store(true, Ordering::Relaxed);
                    task.abort();
                    if let Err(error) = task.await
                        && error.is_panic()
                    {
                        tracing::error!("Preview supervisor panicked: {error}");
                    }
                    tracing::debug!("Cancelled a slow preview while finalizing the recording");
                }
            }
        }
        if !self.whole {
            return None;
        }
        lock_state(&self.state).finalization()
    }
}

impl Drop for PreviewHandle {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Start producing committed utterances and a live tail preview from the
/// auxiliary PCM stream emitted by a batch recording. Only one provider request
/// runs at a time; commits take priority over previews, and audio that arrives
/// meanwhile queues a single rerun rather than replaying every missed interval.
pub fn start(audio_rx: mpsc::Receiver<Arc<[i16]>>, session: PreviewSession) -> PreviewHandle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let sample_rate = session.sample_rate;
    let channels = session.channels;
    let whole = matches!(session.strategy, PreviewStrategy::Whole);
    let min_tail_samples = samples_for_duration(sample_rate, channels, PREVIEW_MIN_AUDIO);
    let max_tail_samples = if session.max_audio.is_zero() {
        usize::MAX
    } else {
        samples_for_duration(sample_rate, channels, session.max_audio)
    };
    let lookback_frames = (samples_for_duration(sample_rate, 1, AGREEMENT_LOOKBACK)) as u64;
    let estimated_lookback_frames =
        (samples_for_duration(sample_rate, 1, ESTIMATED_AGREEMENT_LOOKBACK)) as u64;
    let state = Arc::new(Mutex::new(SegmentedPreview::with_strategy(
        channels,
        min_tail_samples,
        max_tail_samples,
        whole,
        lookback_frames,
        estimated_lookback_frames,
    )));
    let task = tokio::spawn(run(audio_rx, session, cancelled.clone(), state.clone()));
    PreviewHandle {
        task: Some(task),
        cancelled,
        state,
        whole,
    }
}

async fn run(
    mut audio_rx: mpsc::Receiver<Arc<[i16]>>,
    session: PreviewSession,
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<SegmentedPreview>>,
) {
    let PreviewSession {
        sample_rate,
        channels,
        transcriber,
        replacement_rules,
        shared,
        connection,
        generation,
        interval: interval_period,
        max_audio: _,
        strategy,
    } = session;
    let segmented = matches!(strategy, PreviewStrategy::Segmented);
    let whole = !segmented;
    let job_timeout = (interval_period * 5).max(MIN_PREVIEW_JOB_TIMEOUT);
    let mut splitter = OnlineSplitter::new(sample_rate, channels);
    let mut interval = tokio::time::interval(interval_period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // `interval()` ticks immediately. The first useful preview should wait for
    // some speech instead of launching an empty provider request.
    interval.tick().await;

    let mut jobs = JoinSet::<PreviewJobResult>::new();
    let mut in_flight: Option<InFlightKind> = None;
    let mut tail_dirty = false;
    let mut preview_due = false;
    let mut input_closed = false;
    let mut consecutive_failures = 0_u32;
    let mut gave_up_reported = false;
    let mut published = 0_u32;
    let mut reported_failure = false;

    loop {
        if in_flight.is_none() && consecutive_failures < MAX_CONSECUTIVE_FAILURES {
            let next = {
                let mut st = lock_state(&state);
                let commit = if segmented { st.pop_commit() } else { None };
                if let Some((range, audio)) = commit {
                    let frames = range.clone();
                    Some((
                        InFlightKind::Commit(range),
                        RangedSnapshot { audio, frames },
                    ))
                } else if tail_dirty && preview_due {
                    st.take_preview_audio().map(|snapshot| {
                        let decoded_from_frame = snapshot.frames.start;
                        let covered_through_frame = snapshot.frames.end;
                        (
                            InFlightKind::Preview {
                                decoded_from_frame,
                                covered_through_frame,
                            },
                            snapshot,
                        )
                    })
                } else {
                    None
                }
            };
            if let Some((kind, audio)) = next {
                let purpose = match &kind {
                    InFlightKind::Commit(_) => transcriber::Purpose::Final,
                    InFlightKind::Preview { .. } => transcriber::Purpose::Preview,
                };
                if matches!(kind, InFlightKind::Preview { .. }) {
                    tail_dirty = false;
                    preview_due = false;
                }
                spawn_job(
                    &mut jobs,
                    audio,
                    (sample_rate, channels),
                    transcriber.clone(),
                    cancelled.clone(),
                    job_timeout,
                    purpose,
                );
                in_flight = Some(kind);
            } else if input_closed && tail_dirty && preview_due {
                // The closed stream cannot grow enough to cross the minimum
                // preview duration. Mark its one final attempt consumed so
                // the supervisor can finish instead of waiting forever.
                tail_dirty = false;
            }
        }

        let event = tokio::select! {
            chunk = audio_rx.recv(), if !input_closed => Event::Audio(chunk),
            _ = interval.tick() => Event::Interval,
            completed = jobs.join_next(), if !jobs.is_empty() => Event::JobDone(completed),
        };

        match event {
            Event::Audio(chunk) => match chunk {
                Some(samples) => {
                    let mut st = lock_state(&state);
                    if segmented {
                        let closed = splitter.push(&samples);
                        st.enqueue_closed(closed);
                        if splitter.last_push_had_speech() {
                            st.note_speech(splitter.open_region_padded_start());
                        }
                    }
                    st.push_chunk(samples);
                    tail_dirty = true;
                }
                None => {
                    input_closed = true;
                    // One last coalesced pass can cover audio captured since
                    // the previous interval and may be reusable as the final.
                    preview_due = true;
                }
            },
            Event::Interval => preview_due = true,
            Event::JobDone(None) => {
                in_flight = None;
            }
            Event::JobDone(Some(completed)) => {
                let kind = in_flight
                    .take()
                    .expect("a job result arrived with no job in flight");
                match classify(&completed) {
                    JobOutcome::Produced => consecutive_failures = 0,
                    JobOutcome::Failed => consecutive_failures += 1,
                    JobOutcome::Abandoned => {}
                }
                match completed {
                    Ok(Ok(Some(decoded))) => {
                        let decoded_text = decoded.text.clone();
                        let composed = match kind {
                            InFlightKind::Commit(range) => {
                                let composed = {
                                    let mut st = lock_state(&state);
                                    st.commit_succeeded(&range, decoded.text);
                                    compose_live(&st, &replacement_rules)
                                };
                                // The committed cursor moved, so the remaining
                                // tail needs a fresh preview.
                                tail_dirty = true;
                                composed
                            }
                            InFlightKind::Preview {
                                decoded_from_frame,
                                covered_through_frame,
                            } => {
                                let mut st = lock_state(&state);
                                if whole {
                                    st.set_agreed_preview(decoded.words);
                                } else {
                                    st.set_tail_preview(decoded.text);
                                }
                                let raw = st.compose_raw();
                                st.record_decode(
                                    decoded_from_frame,
                                    covered_through_frame.saturating_mul(u64::from(channels)),
                                    &raw,
                                    &decoded_text,
                                );
                                compose_live(&st, &replacement_rules)
                            }
                        };
                        publish_live(&shared, &connection, generation, composed, &mut published)
                            .await;
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        report_failure(&mut reported_failure, format_args!("{error}"));
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        report_failure(&mut reported_failure, format_args!("task failed: {error}"));
                    }
                }
            }
        }

        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES && !gave_up_reported {
            gave_up_reported = true;
            tail_dirty = false;
            tracing::warn!(
                "Disabling preview and commit jobs for this recording after \
                 {MAX_CONSECUTIVE_FAILURES} consecutive failures"
            );
        }

        if cancelled.load(Ordering::Relaxed)
            || (input_closed
                && jobs.is_empty()
                && in_flight.is_none()
                && (!tail_dirty || consecutive_failures >= MAX_CONSECUTIVE_FAILURES))
        {
            break;
        }
    }

    let frames = lock_state(&state).total_frames();
    tracing::debug!(
        "Preview supervisor finished after publishing {published} update(s) over {frames} frame(s)"
    );
}

/// Events the supervisor select loop reacts to.
enum Event {
    Audio(Option<Arc<[i16]>>),
    Interval,
    JobDone(Option<Result<PreviewJobResult, tokio::task::JoinError>>),
}

/// Lock the shared preview state, recovering from a poisoned mutex left by a
/// panicked supervisor instead of losing the recorded audio.
fn lock_state(state: &Arc<Mutex<SegmentedPreview>>) -> std::sync::MutexGuard<'_, SegmentedPreview> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Classify a finished job for the failure budget. Kept separate from the
/// handling so abandoned (cancelled) jobs never count against the provider.
fn classify(completed: &Result<PreviewJobResult, tokio::task::JoinError>) -> JobOutcome {
    match completed {
        Ok(Ok(Some(_))) => JobOutcome::Produced,
        Ok(Ok(None)) => JobOutcome::Abandoned,
        Ok(Err(_)) => JobOutcome::Failed,
        Err(error) if error.is_cancelled() => JobOutcome::Abandoned,
        Err(_) => JobOutcome::Failed,
    }
}

/// How a finished job affects the consecutive-failure budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobOutcome {
    Produced,
    Failed,
    Abandoned,
}

/// Committed utterances, the open tail, and the queue of segments waiting to be
/// transcribed. Pure bookkeeping with no async, so it can be unit tested and
/// safely shared with the handle that collects finalization.
struct SegmentedPreview {
    channels: usize,
    whole: bool,
    committed: Vec<String>,
    agreement: WordAgreementEngine,
    agreement_lookback_frames: u64,
    estimated_agreement_lookback_frames: u64,
    open_start: u64,
    retain_start: u64,
    tail: VecDeque<Arc<[i16]>>,
    tail_base_frame: u64,
    tail_end_frame: u64,
    commit_queue: VecDeque<Range<u64>>,
    tail_preview: String,
    min_tail_samples: usize,
    max_tail_samples: usize,
    speech_since_commit: bool,
    /// Sample count and displayed text of the newest successful decode that
    /// started at frame zero. A tail-only decode must never stand in for the
    /// clean full-file final pass.
    last_decode: Option<(u64, String)>,
}

impl SegmentedPreview {
    #[cfg(test)]
    fn new(channels: u16, min_tail_samples: usize, max_tail_samples: usize) -> Self {
        Self::with_strategy(channels, min_tail_samples, max_tail_samples, false, 0, 0)
    }

    fn with_strategy(
        channels: u16,
        min_tail_samples: usize,
        max_tail_samples: usize,
        whole: bool,
        agreement_lookback_frames: u64,
        estimated_agreement_lookback_frames: u64,
    ) -> Self {
        Self {
            channels: channels.max(1) as usize,
            whole,
            committed: Vec::new(),
            agreement: WordAgreementEngine::new(),
            agreement_lookback_frames,
            estimated_agreement_lookback_frames,
            open_start: 0,
            retain_start: 0,
            tail: VecDeque::new(),
            tail_base_frame: 0,
            tail_end_frame: 0,
            commit_queue: VecDeque::new(),
            tail_preview: String::new(),
            min_tail_samples,
            max_tail_samples,
            speech_since_commit: true,
            last_decode: None,
        }
    }

    /// Record a successful decode of `samples` samples so the final
    /// transcription can reuse it when it already covers the whole recording.
    fn record_decode(
        &mut self,
        decoded_from_frame: u64,
        samples: u64,
        displayed_text: &str,
        decoded_text: &str,
    ) {
        let display_is_complete = displayed_text
            .split_whitespace()
            .eq(decoded_text.split_whitespace());
        self.last_decode = (decoded_from_frame == 0 && display_is_complete)
            .then_some((samples, decoded_text.to_string()));
    }

    /// The newest successful decode: covered sample count and raw text.
    fn finalization(&self) -> Option<(u64, String)> {
        self.last_decode.clone()
    }

    /// Record that newly captured audio contains speech, re-arming tail
    /// previews after a commit. When the tail currently starts with the
    /// silence that followed the commit, the cursor jumps to the new
    /// utterance so neither previews nor the final decode see the gap.
    fn note_speech(&mut self, padded_start: Option<u64>) {
        if let Some(start) = padded_start
            && !self.speech_since_commit
            && start > self.open_start
        {
            // Queued commits still need their audio; never trim past it.
            let keep = self
                .commit_queue
                .front()
                .map(|range| range.start)
                .unwrap_or(u64::MAX);
            let target = start.min(keep);
            if target > self.open_start {
                self.open_start = target;
                self.retain_start = target;
                self.tail_preview.clear();
                self.trim();
            }
        }
        self.speech_since_commit = true;
    }

    fn total_frames(&self) -> u64 {
        self.tail_end_frame
    }

    fn samples_from(&self, start: u64) -> usize {
        (self.total_frames().saturating_sub(start) as usize).saturating_mul(self.channels)
    }

    fn push_chunk(&mut self, chunk: Arc<[i16]>) {
        if chunk.is_empty() {
            return;
        }
        self.tail_end_frame = self
            .tail_end_frame
            .saturating_add((chunk.len() / self.channels) as u64);
        self.tail.push_back(chunk);
        self.trim();
    }

    /// Drop chunks before the earliest frame a future decode can need. Whole
    /// previews retain their configured left lookback; segmented previews
    /// retain from the silence/commit cursor.
    fn trim(&mut self) {
        while let Some(first) = self.tail.front() {
            let frames = (first.len() / self.channels) as u64;
            let chunk_end = self.tail_base_frame + frames;
            if chunk_end > self.retain_start {
                break;
            }
            self.tail_base_frame = chunk_end;
            self.tail.pop_front();
        }
    }

    fn enqueue_closed(&mut self, closed: Vec<Range<u64>>) {
        for range in closed {
            self.commit_queue.push_back(range);
        }
    }

    /// Pop the next queued segment and share every fully covered audio chunk.
    fn pop_commit(&mut self) -> Option<(Range<u64>, AudioSnapshot)> {
        let range = self.commit_queue.pop_front()?;
        let audio = self.snapshot_frames(range.start, range.end);
        Some((range, audio))
    }

    fn commit_succeeded(&mut self, range: &Range<u64>, text: String) {
        self.open_start = self.open_start.max(range.end);
        self.retain_start = self.open_start;
        // A segment that decodes to nothing must not blank the display: the
        // previous hypothesis stays up until a non-empty result replaces it.
        if !text.trim().is_empty() {
            self.committed.push(text);
            self.tail_preview.clear();
        }
        // The tail now starts with the silence that closed the utterance;
        // previewing silence only invites provider hallucinations into the
        // live text until the user speaks again.
        self.speech_since_commit = false;
        self.trim();
    }

    /// Audio for a preview of the open tail, or None when the tail is below the
    /// preview floor, has grown past the preview cap, or holds only silence
    /// since the last commit.
    fn take_preview_audio(&self) -> Option<RangedSnapshot> {
        let start = if self.whole {
            let lookback = if self.agreement.hypothesis_timing_is_exact() {
                self.agreement_lookback_frames
            } else {
                self.estimated_agreement_lookback_frames
            };
            self.agreement
                .hypothesis_start_frame()
                .saturating_sub(lookback)
        } else {
            self.open_start
        };
        let samples = self.samples_from(start);
        if !self.speech_since_commit
            || samples < self.min_tail_samples
            || samples > self.max_tail_samples
        {
            return None;
        }
        let end = self.total_frames();
        Some(RangedSnapshot {
            audio: self.snapshot_frames(start, end),
            frames: start..end,
        })
    }

    fn set_agreed_preview(&mut self, words: Vec<TimedWord>) {
        let result = self.agreement.process(words);
        let display_text = preview_display_text(
            &result.full_text,
            result.hypothesis_stable_to_end || self.tail_preview == result.full_text,
        );
        let has_confirmed_prefix = !self.agreement.confirmed_text().is_empty();
        let dropped = dropped_preview_words(&self.tail_preview, &display_text);
        if !display_text.is_empty()
            && (!has_confirmed_prefix || dropped <= MAX_LATE_HYPOTHESIS_DROPS)
        {
            self.tail_preview = display_text;
        }
        self.retain_start = self.agreement.hypothesis_start_frame().saturating_sub(
            if self.agreement.hypothesis_timing_is_exact() {
                self.agreement_lookback_frames
            } else {
                self.estimated_agreement_lookback_frames
            },
        );
        self.trim();
    }

    fn set_tail_preview(&mut self, text: String) {
        // An empty decode is a failed hypothesis, not news: blanking the live
        // text over it would flash the placeholder between every real result.
        if text.trim().is_empty() {
            return;
        }
        self.tail_preview = text;
    }

    /// Share complete chunks in [start, end), copying only partial boundaries.
    fn snapshot_frames(&self, start: u64, end: u64) -> AudioSnapshot {
        let start = start.max(self.tail_base_frame);
        let end = end.min(self.total_frames());
        if end <= start {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor = self.tail_base_frame;
        for chunk in &self.tail {
            let frames = (chunk.len() / self.channels) as u64;
            let chunk_start = cursor;
            let chunk_end = cursor + frames;
            cursor = chunk_end;
            if chunk_end <= start || chunk_start >= end {
                continue;
            }
            let from = (start.max(chunk_start) - chunk_start) as usize * self.channels;
            let to = (end.min(chunk_end) - chunk_start) as usize * self.channels;
            if from == 0 && to == chunk.len() {
                out.push(chunk.clone());
            } else {
                out.push(Arc::from(chunk[from..to].to_vec()));
            }
        }
        out
    }

    /// Raw live text before dictionary correction: committed utterances followed
    /// by the current tail hypothesis, empty pieces skipped.
    fn compose_raw(&self) -> String {
        let mut parts = self
            .committed
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        parts.push(&self.tail_preview);
        parts
            .into_iter()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn preview_display_text(text: &str, stable_to_end: bool) -> String {
    let mut words = text.split_whitespace().collect::<Vec<_>>();
    let ends_sentence = text
        .trim_end()
        .chars()
        .next_back()
        .is_some_and(|character| {
            matches!(character, '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；')
        });
    if words.len() >= 5 && !ends_sentence && !stable_to_end {
        words.pop();
    }
    words.join(" ")
}

fn dropped_preview_words(previous: &str, current: &str) -> usize {
    let mut current_counts = HashMap::<String, usize>::new();
    for word in normalized_preview_words(current) {
        *current_counts.entry(word).or_default() += 1;
    }
    normalized_preview_words(previous)
        .into_iter()
        .filter(|word| match current_counts.get_mut(word) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .count()
}

fn normalized_preview_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.chars()
                .flat_map(char::to_lowercase)
                .filter(|character| character.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Apply the dictionary to the composed preview so the live text matches the
/// correction the final transcript will get.
fn compose_live(
    state: &SegmentedPreview,
    replacement_rules: &[voxkey_ipc::WordReplacement],
) -> String {
    correct_preview(&state.compose_raw(), replacement_rules)
}

/// Publish a corrected live transcript if it changed, counting the update.
async fn publish_live(
    shared: &SharedState,
    connection: &zbus::Connection,
    generation: u64,
    text: String,
    published: &mut u32,
) {
    if shared.update_live_transcript(generation, text) {
        DaemonInterface::notify_live_transcript(connection).await;
        *published += 1;
    }
}

/// Reuse the newest preview decode as the final transcript when it already
/// covers all microphone samples in the finalized recording. Synthetic
/// transcription padding is intentionally excluded. The preview shows the text
/// that gets inserted, instead of two independent decodes of the same model
/// disagreeing. Any gap (audio captured after the last preview, or preview
/// chunks dropped under backpressure) forces a fresh whole-file decode.
pub(crate) fn reusable_preview_final(
    finalization: Option<(u64, String)>,
    captured_samples: u64,
    dropped_chunks: u64,
) -> Option<String> {
    let (covered, text) = finalization?;
    if dropped_chunks == 0 && covered == captured_samples {
        Some(text)
    } else {
        None
    }
}

/// Warn about the first preview failure of a recording and keep the rest at
/// debug. A provider failing every interval must not flood the log.
fn report_failure(already_reported: &mut bool, error: std::fmt::Arguments<'_>) {
    if *already_reported {
        tracing::debug!("Preview transcription failed again: {error}");
    } else {
        *already_reported = true;
        tracing::warn!("Preview transcription failed: {error}");
    }
}

/// Spawn one transcription job. `pcm_format` is (sample_rate, channels).
fn spawn_job(
    jobs: &mut JoinSet<PreviewJobResult>,
    snapshot: RangedSnapshot,
    pcm_format: (u32, u16),
    transcriber: Arc<Transcriber>,
    cancelled: Arc<AtomicBool>,
    job_timeout: Duration,
    purpose: transcriber::Purpose,
) {
    let (sample_rate, channels) = pcm_format;
    jobs.spawn(async move {
        decode_audio(
            snapshot,
            sample_rate,
            channels,
            &transcriber,
            cancelled,
            job_timeout,
            purpose,
        )
        .await
    });
}

/// Transcribe one audio snapshot, returning Ok(None) when cancelled before the
/// provider could be contacted so the caller does not blame the backend.
async fn decode_audio(
    snapshot: RangedSnapshot,
    sample_rate: u32,
    channels: u16,
    transcriber: &Transcriber,
    cancelled: Arc<AtomicBool>,
    job_timeout: Duration,
    purpose: transcriber::Purpose,
) -> PreviewJobResult {
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let RangedSnapshot { mut audio, frames } = snapshot;

    // Backends that take PCM directly skip writing a WAV the daemon would
    // only have them read straight back: the samples are already here.
    if transcriber.accepts_pcm() {
        audio.push(Arc::from(vec![
            0_i16;
            samples_for_duration(
                sample_rate,
                channels,
                TRAILING_SILENCE
            )
        ]));
        return transcriber
            .transcribe_pcm_detailed(job_timeout, purpose, sample_rate, channels, &audio)
            .await
            .map(|decoded| Some(absolutize_decode(decoded, &frames)))
            .map_err(|error| error.to_string());
    }

    let audio_path = tokio::task::spawn_blocking(move || {
        write_snapshot_wav(&audio, sample_rate, channels).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())??;

    // Providers such as local Parakeet run inference on a blocking thread
    // that cannot be aborted. Checking here keeps a doomed job from holding
    // the recognizer while the final transcription waits for it.
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let text = transcriber
        .transcribe_padded_within(job_timeout, purpose, &audio_path)
        .await
        .map_err(|error| error.to_string())?;
    let text = crate::dictionary::filter_transcription_output(&text);
    let words = estimate_word_timings(&text, frames.start, frames.end);
    Ok(Some(DecodedPreview { text, words }))
}

fn absolutize_decode(
    decoded: transcriber::DecodedTranscript,
    frames: &Range<u64>,
) -> DecodedPreview {
    let text = crate::dictionary::filter_transcription_output(&decoded.text);
    let changed_by_filter = text != decoded.text.trim();
    let mut words = if changed_by_filter {
        Vec::new()
    } else {
        decoded.words
    };
    for word in &mut words {
        word.start_frame = frames
            .start
            .saturating_add(word.start_frame)
            .min(frames.end);
        word.end_frame = frames.start.saturating_add(word.end_frame).min(frames.end);
    }
    if words.is_empty() {
        words = estimate_word_timings(&text, frames.start, frames.end);
    }
    DecodedPreview { text, words }
}

fn samples_for_duration(sample_rate: u32, channels: u16, duration: Duration) -> usize {
    let frames = (sample_rate as u128 * duration.as_millis()) / 1000;
    frames
        .saturating_mul(channels as u128)
        .min(usize::MAX as u128) as usize
}

fn wav_sample_count(total: usize) -> Result<u32, std::io::Error> {
    u32::try_from(total).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "preview audio is too large for a WAV file",
        )
    })
}

/// Total interleaved i16 samples stored in a finalized WAV file, matching how
/// preview snapshots count their own samples.
#[cfg(test)]
pub(crate) fn wav_file_sample_count(path: &std::path::Path) -> Result<u64, hound::Error> {
    let reader = hound::WavReader::open(path)?;
    Ok(u64::from(reader.len()))
}

/// Write one immutable audio snapshot. `TempPath` removes the file if a
/// provider future is cancelled; successful transcriptions already delete it.
fn write_snapshot_wav(
    snapshot: &AudioSnapshot,
    sample_rate: u32,
    channels: u16,
) -> Result<tempfile::TempPath, Box<dyn std::error::Error + Send + Sync>> {
    let temp_path = tempfile::Builder::new()
        .prefix("voxkey_preview_")
        .suffix(".wav")
        .tempfile()?
        .into_temp_path();
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&temp_path, spec)?;
    {
        let audio_samples = snapshot.iter().try_fold(0_usize, |total, chunk| {
            total.checked_add(chunk.len()).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "preview audio sample count overflowed",
                )
            })
        })?;
        let silence_samples = samples_for_duration(sample_rate, channels, TRAILING_SILENCE);
        let total = audio_samples.checked_add(silence_samples).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preview audio sample count overflowed",
            )
        })?;
        // The bulk i16 writer skips per-sample dispatch and bounds checks,
        // which matters when a snapshot runs to millions of samples.
        let mut samples = writer.get_i16_writer(wav_sample_count(total)?);
        for chunk in snapshot {
            for sample in chunk.iter() {
                samples.write_sample(*sample);
            }
        }
        for _ in 0..silence_samples {
            samples.write_sample(0);
        }
        samples.flush()?;
    }
    writer.finalize()?;
    Ok(temp_path)
}

fn correct_preview(transcript: &str, replacement_rules: &[voxkey_ipc::WordReplacement]) -> String {
    crate::dictionary::process_transcription_output(transcript, replacement_rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(samples: &[i16]) -> Arc<[i16]> {
        Arc::from(samples.to_vec())
    }

    fn closed(range: Range<u64>) -> Vec<Range<u64>> {
        std::iter::once(range).collect()
    }

    fn timed_words(text: &str) -> Vec<TimedWord> {
        text.split_whitespace()
            .enumerate()
            .map(|(index, word)| TimedWord::new(word, index as u64 * 10, index as u64 * 10 + 9))
            .collect()
    }

    fn state(max_tail_samples: usize) -> SegmentedPreview {
        SegmentedPreview::new(1, 1, max_tail_samples)
    }

    #[test]
    fn commit_advances_the_cursor_and_trims_the_tail() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
        sp.enqueue_closed(closed(0..4));

        let (range, audio) = sp.pop_commit().unwrap();
        assert_eq!(range, 0..4);
        assert_eq!(&*audio[0], &[1, 2, 3, 4]);

        sp.commit_succeeded(&range, "hello".to_string());
        assert_eq!(sp.open_start, 4);
        assert_eq!(sp.committed, vec!["hello".to_string()]);

        // The tail kept for later previews starts just past the committed audio.
        sp.note_speech(None);
        let tail = sp.take_preview_audio().unwrap();
        let flat: Vec<i16> = tail.audio.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(flat, vec![5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn a_failed_commit_leaves_its_region_in_the_tail() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1, 2, 3, 4, 5]));
        sp.enqueue_closed(closed(0..3));

        let (_range, _audio) = sp.pop_commit().unwrap();
        // No commit_succeeded call: the provider failed.

        assert!(sp.committed.is_empty());
        assert_eq!(sp.open_start, 0);
        let tail = sp.take_preview_audio().unwrap();
        let flat: Vec<i16> = tail.audio.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(flat, vec![1, 2, 3, 4, 5], "nothing may be dropped");
    }

    #[test]
    fn live_text_orders_committed_before_the_tail_preview() {
        let mut sp = state(usize::MAX);
        sp.committed = vec!["one".to_string(), "two".to_string()];

        sp.set_tail_preview("three".to_string());
        assert_eq!(sp.compose_raw(), "one two three");

        let mut tail_only = state(usize::MAX);
        tail_only.set_tail_preview("only".to_string());
        assert_eq!(tail_only.compose_raw(), "only");
    }

    #[test]
    fn an_empty_preview_hypothesis_keeps_the_previous_text() {
        let mut sp = state(usize::MAX);
        sp.set_tail_preview("hello world".to_string());

        sp.set_tail_preview(String::new());
        assert_eq!(
            sp.compose_raw(),
            "hello world",
            "an empty decode must not erase words already on screen"
        );

        sp.set_tail_preview("   ".to_string());
        assert_eq!(sp.compose_raw(), "hello world");

        sp.set_tail_preview("hello world again".to_string());
        assert_eq!(sp.compose_raw(), "hello world again");
    }

    #[test]
    fn an_empty_commit_result_keeps_the_previous_text() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1, 2, 3, 4, 5, 6]));
        sp.set_tail_preview("hello".to_string());
        sp.enqueue_closed(closed(0..3));

        let (range, _) = sp.pop_commit().unwrap();
        sp.commit_succeeded(&range, String::new());

        assert_eq!(
            sp.open_start, 3,
            "the cursor still advances past the segment"
        );
        assert_eq!(
            sp.compose_raw(),
            "hello",
            "a segment that decodes to nothing must not blank the display"
        );
    }

    #[test]
    fn without_commits_previews_cover_the_whole_recording() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1, 2, 3]));
        sp.push_chunk(chunk(&[4, 5, 6]));

        let audio = sp.take_preview_audio().unwrap();
        let flat: Vec<i16> = audio.audio.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(flat, vec![1, 2, 3, 4, 5, 6]);

        sp.set_tail_preview("whole stream text".to_string());
        assert_eq!(sp.compose_raw(), "whole stream text");
    }

    #[test]
    fn snapshots_reuse_complete_chunks_and_copy_only_boundaries() {
        let first = chunk(&[1, 2, 3, 4]);
        let middle = chunk(&[5, 6, 7, 8]);
        let last = chunk(&[9, 10, 11, 12]);
        let mut sp = state(usize::MAX);
        sp.push_chunk(first.clone());
        sp.push_chunk(middle.clone());
        sp.push_chunk(last.clone());

        let whole = sp.snapshot_frames(0, 12);
        assert_eq!(whole.len(), 3);
        assert!(Arc::ptr_eq(&whole[0], &first));
        assert!(Arc::ptr_eq(&whole[1], &middle));
        assert!(Arc::ptr_eq(&whole[2], &last));

        let partial = sp.snapshot_frames(2, 10);
        assert_eq!(partial.len(), 3);
        assert_eq!(&*partial[0], &[3, 4]);
        assert!(Arc::ptr_eq(&partial[1], &middle));
        assert_eq!(&*partial[2], &[9, 10]);
        assert!(!Arc::ptr_eq(&partial[0], &first));
        assert!(!Arc::ptr_eq(&partial[2], &last));
    }

    #[test]
    fn cached_frame_count_survives_large_constant_time_trim() {
        let mut sp = state(usize::MAX);
        for sample in 0..10_000_i16 {
            sp.push_chunk(chunk(&[sample]));
        }
        assert_eq!(sp.total_frames(), 10_000);

        sp.retain_start = 9_999;
        sp.trim();

        assert_eq!(sp.tail_base_frame, 9_999);
        assert_eq!(sp.tail.len(), 1);
        assert_eq!(sp.total_frames(), 10_000);
        sp.push_chunk(chunk(&[10_000]));
        assert_eq!(sp.total_frames(), 10_001);
    }

    #[test]
    fn whole_previews_resume_at_the_first_hypothesis_with_left_context() {
        let mut sp = SegmentedPreview::with_strategy(1, 1, usize::MAX, true, 15, 30);
        for block in 0_i16..12 {
            sp.push_chunk(chunk(&(block * 10..block * 10 + 10).collect::<Vec<_>>()));
        }
        let words = || {
            "one two three four five. six seven eight. nine ten eleven."
                .split_whitespace()
                .enumerate()
                .map(|(index, text)| TimedWord::new(text, index as u64 * 10, index as u64 * 10 + 9))
                .collect::<Vec<_>>()
        };

        for _ in 0..3 {
            sp.set_agreed_preview(words());
        }

        let snapshot = sp.take_preview_audio().unwrap();
        let flat = snapshot
            .audio
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(snapshot.frames, 35..120);
        assert_eq!(flat, (35_i16..120).collect::<Vec<_>>());
        assert_eq!(
            sp.compose_raw(),
            "one two three four five. six seven eight. nine ten eleven."
        );
        assert_eq!(sp.tail_base_frame, 30, "old full chunks should be released");
    }

    #[test]
    fn a_confirmed_whole_preview_ignores_a_catastrophic_late_regression() {
        let mut sp = SegmentedPreview::with_strategy(1, 1, usize::MAX, true, 15, 30);
        let stable = concat!(
            "one two three four five. six seven eight nine ten. ",
            "eleven twelve thirteen fourteen fifteen. ",
            "sixteen seventeen eighteen nineteen twenty."
        );
        for _ in 0..3 {
            sp.set_agreed_preview(timed_words(stable));
        }
        assert_eq!(sp.compose_raw(), stable);

        sp.set_agreed_preview(timed_words(
            "one two three four five. six seven eight nine ten. prompt echo only.",
        ));

        assert_eq!(sp.compose_raw(), stable);
    }

    #[test]
    fn whole_preview_holds_one_open_word_but_keeps_short_commands() {
        assert_eq!(preview_display_text("yes", false), "yes");
        assert_eq!(
            preview_display_text("one two three four partial", false),
            "one two three four"
        );
        assert_eq!(
            preview_display_text("one two three four complete.", false),
            "one two three four complete."
        );
        assert_eq!(
            preview_display_text("one two three four stable", true),
            "one two three four stable"
        );
    }

    #[test]
    fn finalization_tracks_the_latest_successful_decode() {
        let mut sp = state(usize::MAX);
        assert!(sp.finalization().is_none());

        sp.record_decode(0, 100, "first", "first");
        sp.record_decode(0, 250, "second", "second");

        assert_eq!(sp.finalization(), Some((250, "second".to_string())));

        sp.record_decode(10, 300, "tail-only", "tail-only");
        assert_eq!(
            sp.finalization(),
            None,
            "a decode that sought past frame zero cannot replace the final pass"
        );

        sp.record_decode(0, 400, "stable prefix", "stable prefix plus hypothesis");
        assert_eq!(
            sp.finalization(),
            None,
            "an unconfirmed tail cannot be mistaken for a complete final"
        );
    }

    #[test]
    fn only_a_fresh_complete_decode_is_reusable_as_the_final_transcript() {
        let finalization = Some((10, "hello".to_string()));
        assert_eq!(
            reusable_preview_final(finalization.clone(), 10, 0),
            Some("hello".to_string())
        );
        assert_eq!(
            reusable_preview_final(finalization.clone(), 12, 0),
            None,
            "audio arrived after the last preview"
        );
        assert_eq!(
            reusable_preview_final(finalization.clone(), 10, 1),
            None,
            "dropped chunks mean the preview heard less than the file"
        );
        assert_eq!(reusable_preview_final(None, 10, 0), None);
    }

    #[test]
    fn commit_clears_the_stale_tail_preview() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1, 2, 3, 4, 5, 6]));
        sp.set_tail_preview("old tail".to_string());
        sp.enqueue_closed(closed(0..3));

        let (range, _) = sp.pop_commit().unwrap();
        sp.commit_succeeded(&range, "committed".to_string());

        assert_eq!(sp.compose_raw(), "committed");
    }

    #[test]
    fn tail_previews_pause_until_speech_resumes_after_a_commit() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1, 2, 3]));
        assert!(
            sp.take_preview_audio().is_some(),
            "no commit yet, previews run"
        );

        sp.enqueue_closed(closed(0..3));
        let (range, _) = sp.pop_commit().unwrap();
        sp.commit_succeeded(&range, "done".to_string());
        sp.push_chunk(chunk(&[0, 0, 0]));
        assert!(
            sp.take_preview_audio().is_none(),
            "a silent tail after a commit must not be previewed"
        );

        sp.note_speech(None);
        assert!(
            sp.take_preview_audio().is_some(),
            "new speech re-arms tail previews"
        );
    }

    #[test]
    fn resuming_speech_skips_the_silence_that_followed_the_commit() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1; 10]));
        sp.enqueue_closed(closed(0..5));
        let (range, _) = sp.pop_commit().unwrap();
        sp.commit_succeeded(&range, "first".to_string());
        sp.push_chunk(chunk(&[0; 10]));

        // The new utterance starts at frame 20; the tail cursor must jump past
        // the silence so previews never see the gap.
        sp.note_speech(Some(20));

        assert_eq!(sp.open_start, 20);
        sp.push_chunk(chunk(&[5; 6]));
        let tail = sp.take_preview_audio().unwrap();
        let flat: Vec<i16> = tail.audio.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(flat, vec![5; 6], "the silent gap must not be in the tail");
    }

    #[test]
    fn silence_skip_never_trims_queued_commit_audio() {
        let mut sp = state(usize::MAX);
        sp.push_chunk(chunk(&[1; 10]));
        sp.enqueue_closed(closed(0..5));
        sp.push_chunk(chunk(&[0; 10]));
        let (range, _) = sp.pop_commit().unwrap();
        sp.commit_succeeded(&range, "a".to_string());

        sp.push_chunk(chunk(&[2; 10]));
        sp.enqueue_closed(closed(20..30));
        sp.push_chunk(chunk(&[0; 10]));

        // A new utterance begins at frame 40 while the second commit is still
        // queued; the cursor may only jump as far as that commit's audio.
        sp.note_speech(Some(40));

        assert_eq!(sp.open_start, 20);
        let (range_b, audio) = sp.pop_commit().unwrap();
        assert_eq!(range_b, 20..30);
        assert_eq!(&*audio[0], &[2; 10]);
    }

    #[test]
    fn preview_is_skipped_below_the_floor_and_past_the_cap() {
        let mut sp = SegmentedPreview::new(1, 3, 4);
        assert!(sp.take_preview_audio().is_none(), "empty tail");

        sp.push_chunk(chunk(&[1, 2]));
        assert!(
            sp.take_preview_audio().is_none(),
            "tail below the floor is not previewed"
        );

        sp.push_chunk(chunk(&[3]));
        assert!(sp.take_preview_audio().is_some(), "tail at the floor");

        sp.push_chunk(chunk(&[4]));
        assert!(sp.take_preview_audio().is_some(), "tail at the cap");

        sp.push_chunk(chunk(&[5]));
        assert!(
            sp.take_preview_audio().is_none(),
            "tail past the cap is not previewed"
        );
    }

    #[test]
    fn preview_text_uses_the_same_dictionary_replacements_as_final_text() {
        let rules = vec![voxkey_ipc::WordReplacement {
            original: "voice ink".to_string(),
            replacement: "VoiceInk".to_string(),
            enabled: true,
        }];

        assert_eq!(
            correct_preview("using voice ink now", &rules),
            "using VoiceInk now"
        );
    }

    #[test]
    fn snapshot_wav_preserves_the_recording_format_and_samples() {
        let snapshot: AudioSnapshot = vec![chunk(&[-32768, -1, 0, 32767])];
        let path = write_snapshot_wav(&snapshot, 16_000, 1).unwrap();
        let mut reader = hound::WavReader::open(&path).unwrap();

        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().channels, 1);
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(&samples[..4], &[-32768, -1, 0, 32767]);
        assert_eq!(samples.len(), 16_004);
        assert!(samples[4..].iter().all(|sample| *sample == 0));
    }

    #[test]
    fn sample_count_accounts_for_every_channel() {
        assert_eq!(
            samples_for_duration(16_000, 1, Duration::from_millis(1200)),
            19_200
        );
        assert_eq!(
            samples_for_duration(16_000, 2, Duration::from_millis(1200)),
            38_400
        );
    }

    #[test]
    fn wav_file_sample_count_matches_the_written_samples() {
        let snapshot: AudioSnapshot = vec![chunk(&[1, 2, 3, 4, 5, 6])];
        let path = write_snapshot_wav(&snapshot, 16_000, 2).unwrap();
        assert_eq!(wav_file_sample_count(&path).unwrap(), 32_006);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn oversized_preview_sample_counts_do_not_wrap_in_the_wav_header() {
        let total = u32::MAX as usize + 1;

        assert!(wav_sample_count(total).is_err());
    }

    #[tokio::test]
    async fn cancelled_jobs_never_reach_the_provider() {
        let mut jobs = JoinSet::<PreviewJobResult>::new();
        let cancelled = Arc::new(AtomicBool::new(true));
        let transcriber = Arc::new(Transcriber::WhisperCpp {
            command: "/nonexistent/voxkey-preview-should-not-run".to_string(),
            args: vec!["{audio_file}".to_string()],
        });

        spawn_job(
            &mut jobs,
            RangedSnapshot {
                audio: vec![chunk(&[0; 16])],
                frames: 0..16,
            },
            (16_000, 1),
            transcriber,
            cancelled,
            MIN_PREVIEW_JOB_TIMEOUT,
            transcriber::Purpose::Preview,
        );

        assert_eq!(jobs.join_next().await.unwrap().unwrap(), Ok(None));
    }
}
