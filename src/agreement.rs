// ABOUTME: Confirms only words that agree across three consecutive preview decodes.
// ABOUTME: Advances confirmation at sentence boundaries while retaining two live sentences.

use std::collections::VecDeque;

const AGREEMENT_PASSES: usize = 3;
const MIN_WORDS_TO_CONFIRM: usize = 5;
const RETAINED_SENTENCES: usize = 2;
const MIN_BOUNDARY_CONFIDENCE: f32 = 0.6;
const MIN_CONTEXT_ANCHOR_WORDS: usize = 3;
const MAX_CONTEXT_ANCHOR_WORDS: usize = 5;
const MAX_CONTEXT_ANCHOR_SEARCH_WORDS: usize = 24;

/// One decoded word with its location relative to the complete recording.
/// Confidence is optional because sherpa-onnx's safe Rust wrapper currently
/// exposes token timestamps but not its native per-token log probabilities.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TimedWord {
    pub text: String,
    pub start_frame: u64,
    pub end_frame: u64,
    pub confidence: Option<f32>,
    pub timing_is_exact: bool,
}

impl TimedWord {
    pub(crate) fn new(text: impl Into<String>, start_frame: u64, end_frame: u64) -> Self {
        Self {
            text: text.into(),
            start_frame,
            end_frame,
            confidence: None,
            timing_is_exact: true,
        }
    }

    fn estimated(text: impl Into<String>, start_frame: u64, end_frame: u64) -> Self {
        Self {
            timing_is_exact: false,
            ..Self::new(text, start_frame, end_frame)
        }
    }

    fn normalized(&self) -> String {
        self.text
            .chars()
            .flat_map(char::to_lowercase)
            .map(|character| if character == '-' { ' ' } else { character })
            .filter(|character| character.is_alphanumeric() || character.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgreementResult {
    pub full_text: String,
    pub hypothesis_text: String,
    pub newly_confirmed_text: String,
    pub hypothesis_stable_to_end: bool,
}

/// Stateful three-pass prefix agreement. Confirmed words are immutable; only
/// the unconfirmed hypothesis supplied after them can change on later passes.
pub(crate) struct WordAgreementEngine {
    confirmed_words: Vec<TimedWord>,
    recent_hypotheses: VecDeque<Vec<TimedWord>>,
    confirmed_end_frame: u64,
    hypothesis_start_frame: u64,
    hypothesis_timing_is_exact: bool,
    hypothesis_stable_to_end: bool,
}

impl WordAgreementEngine {
    pub(crate) fn new() -> Self {
        Self {
            confirmed_words: Vec::new(),
            recent_hypotheses: VecDeque::with_capacity(AGREEMENT_PASSES),
            confirmed_end_frame: 0,
            hypothesis_start_frame: 0,
            hypothesis_timing_is_exact: false,
            hypothesis_stable_to_end: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn confirmed_end_frame(&self) -> u64 {
        self.confirmed_end_frame
    }

    pub(crate) fn hypothesis_start_frame(&self) -> u64 {
        self.hypothesis_start_frame
    }

    pub(crate) fn hypothesis_timing_is_exact(&self) -> bool {
        self.hypothesis_timing_is_exact
    }

    pub(crate) fn confirmed_text(&self) -> String {
        join_words(&self.confirmed_words)
    }

    /// Fold one complete decode of the current window into the agreement
    /// history. Audio lookback may make the decode repeat already-confirmed
    /// words; those are discarded by text overlap and timestamps first.
    pub(crate) fn process(&mut self, words: Vec<TimedWord>) -> AgreementResult {
        let words = self.discard_confirmed_context(words);

        // Treat an empty pass as unavailable evidence. Keeping the last
        // hypothesis avoids flashing or dropping words on a silence-only or
        // transiently failed decode.
        if words.is_empty() {
            let hypothesis = self.recent_hypotheses.back().cloned().unwrap_or_default();
            return self.make_result(&hypothesis, &[]);
        }

        self.hypothesis_start_frame = words
            .first()
            .map_or(self.confirmed_end_frame, |word| word.start_frame);
        self.hypothesis_timing_is_exact = words.iter().all(|word| word.timing_is_exact);
        self.recent_hypotheses.push_back(words.clone());
        if self.recent_hypotheses.len() > AGREEMENT_PASSES {
            self.recent_hypotheses.pop_front();
        }

        if self.recent_hypotheses.len() < AGREEMENT_PASSES {
            self.hypothesis_stable_to_end = false;
            return self.make_result(&words, &[]);
        }

        let common_prefix_len = longest_common_prefix(&self.recent_hypotheses);
        self.hypothesis_stable_to_end = common_prefix_len == words.len();
        if common_prefix_len < MIN_WORDS_TO_CONFIRM {
            return self.make_result(&words, &[]);
        }

        let confirm_count = sentence_boundary_cut(&words[..common_prefix_len]);
        if confirm_count < MIN_WORDS_TO_CONFIRM
            || !boundary_confidence_allows(&words[..confirm_count])
        {
            return self.make_result(&words, &[]);
        }

        let newly_confirmed = words[..confirm_count].to_vec();
        let hypothesis = words[confirm_count..].to_vec();
        self.confirmed_end_frame = newly_confirmed
            .last()
            .map_or(self.confirmed_end_frame, |word| word.end_frame);
        self.hypothesis_start_frame = hypothesis
            .first()
            .map_or(self.confirmed_end_frame, |word| word.start_frame);
        self.hypothesis_timing_is_exact = hypothesis.iter().all(|word| word.timing_is_exact)
            && newly_confirmed.iter().all(|word| word.timing_is_exact);
        self.confirmed_words.extend(newly_confirmed.iter().cloned());

        // The remainder has appeared once in this pass. It must still appear
        // in two more passes before any part of it can be confirmed.
        self.recent_hypotheses.clear();
        if !hypothesis.is_empty() {
            self.recent_hypotheses.push_back(hypothesis.clone());
        }

        self.make_result(&hypothesis, &newly_confirmed)
    }

    /// Remove the left context deliberately included before the first
    /// unconfirmed word. Text overlap handles providers without trustworthy
    /// timestamps; frame bounds handle a decode that rewrites the overlap.
    fn discard_confirmed_context(&self, mut words: Vec<TimedWord>) -> Vec<TimedWord> {
        if self.confirmed_words.is_empty() || words.is_empty() {
            return words;
        }

        let max_overlap = self.confirmed_words.len().min(words.len());
        let overlap = (1..=max_overlap)
            .rev()
            .find(|&length| {
                self.confirmed_words[self.confirmed_words.len() - length..]
                    .iter()
                    .zip(&words[..length])
                    .all(|(confirmed, current)| confirmed.normalized() == current.normalized())
            })
            .unwrap_or(0);
        words.drain(..overlap);

        // The acoustic lookback can be recognized differently from the pass
        // that produced the confirmation boundary. Anchor on the beginning of
        // the previous unconfirmed hypothesis and discard only words before
        // that anchor, so rewritten context cannot be inserted twice while
        // genuine words after the boundary remain intact.
        if let Some(previous) = self.recent_hypotheses.back() {
            let anchor_len = previous.len().min(MAX_CONTEXT_ANCHOR_WORDS);
            if anchor_len >= MIN_CONTEXT_ANCHOR_WORDS {
                let search_len = words
                    .len()
                    .min(MAX_CONTEXT_ANCHOR_SEARCH_WORDS + anchor_len);
                if let Some(anchor_start) =
                    words[..search_len].windows(anchor_len).position(|window| {
                        window
                            .iter()
                            .zip(previous)
                            .all(|(current, prior)| current.normalized() == prior.normalized())
                    })
                {
                    words.drain(..anchor_start);
                }
            }
        }

        // Exact Parakeet token timestamps can reject a rewritten overlap that
        // text matching did not recognize. Whisper subprocess/HTTP timings
        // are only distributed estimates: using them as cut evidence can
        // delete genuinely new words when the window grows or seek position
        // shifts between passes.
        if words.first().is_some_and(|word| word.timing_is_exact) {
            let first_unconfirmed = words
                .iter()
                .position(|word| word.end_frame > self.confirmed_end_frame)
                .unwrap_or(words.len());
            words.drain(..first_unconfirmed);
        }
        words
    }

    fn make_result(
        &self,
        hypothesis: &[TimedWord],
        newly_confirmed: &[TimedWord],
    ) -> AgreementResult {
        let confirmed_text = self.confirmed_text();
        let hypothesis_text = join_words(hypothesis);
        let full_text = [confirmed_text.as_str(), hypothesis_text.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        AgreementResult {
            full_text,
            hypothesis_text,
            newly_confirmed_text: join_words(newly_confirmed),
            hypothesis_stable_to_end: self.hypothesis_stable_to_end,
        }
    }
}

fn join_words(words: &[TimedWord]) -> String {
    words
        .iter()
        .map(|word| word.text.trim())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn longest_common_prefix(hypotheses: &VecDeque<Vec<TimedWord>>) -> usize {
    let shortest = hypotheses.iter().map(Vec::len).min().unwrap_or(0);
    (0..shortest)
        .take_while(|&index| {
            let expected = hypotheses[0][index].normalized();
            hypotheses
                .iter()
                .skip(1)
                .all(|words| words[index].normalized() == expected)
        })
        .count()
}

/// Return a word count that leaves the newest two complete sentences live.
fn sentence_boundary_cut(words: &[TimedWord]) -> usize {
    let punctuation_indices = words
        .iter()
        .enumerate()
        .filter_map(|(index, word)| {
            word.text
                .chars()
                .next_back()
                .filter(|character| {
                    matches!(character, '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；')
                })
                .map(|_| index)
        })
        .collect::<Vec<_>>();
    if punctuation_indices.len() <= RETAINED_SENTENCES {
        return 0;
    }
    punctuation_indices[punctuation_indices.len() - RETAINED_SENTENCES - 1] + 1
}

fn boundary_confidence_allows(words: &[TimedWord]) -> bool {
    // Gate the cut and its two preceding words when the backend supplies
    // scores. A missing score means the capability is unavailable, not zero
    // confidence; sherpa-onnx 1.13 exposes timestamps but not log-probability
    // scores through its safe Rust result type.
    words.iter().rev().take(3).all(|word| {
        word.confidence
            .is_none_or(|score| score >= MIN_BOUNDARY_CONFIDENCE)
    })
}

/// Make conservative timings for text-only providers. Exact token timestamps
/// from Parakeet replace these; subprocess and HTTP backends still get a safe
/// seek point with an additional audio lookback in the preview layer.
pub(crate) fn estimate_word_timings(
    text: &str,
    start_frame: u64,
    end_frame: u64,
) -> Vec<TimedWord> {
    let pieces = text.split_whitespace().collect::<Vec<_>>();
    if pieces.is_empty() {
        return Vec::new();
    }
    let span = end_frame.saturating_sub(start_frame);
    pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let word_start = start_frame + span.saturating_mul(index as u64) / pieces.len() as u64;
            let word_end =
                start_frame + span.saturating_mul((index + 1) as u64) / pieces.len() as u64;
            TimedWord::estimated(*piece, word_start, word_end)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str, offset: u64) -> Vec<TimedWord> {
        text.split_whitespace()
            .enumerate()
            .map(|(index, text)| {
                TimedWord::new(
                    text,
                    offset + index as u64 * 10,
                    offset + index as u64 * 10 + 9,
                )
            })
            .collect()
    }

    const THREE_SENTENCES: &str = "one two three four five. six seven eight. nine ten eleven.";

    #[test]
    fn three_identical_passes_confirm_only_through_the_third_last_ender() {
        let mut engine = WordAgreementEngine::new();
        assert!(
            engine
                .process(words(THREE_SENTENCES, 0))
                .newly_confirmed_text
                .is_empty()
        );
        assert!(
            engine
                .process(words(THREE_SENTENCES, 0))
                .newly_confirmed_text
                .is_empty()
        );

        let result = engine.process(words(THREE_SENTENCES, 0));

        assert_eq!(result.newly_confirmed_text, "one two three four five.");
        assert_eq!(result.hypothesis_text, "six seven eight. nine ten eleven.");
        assert_eq!(result.full_text, THREE_SENTENCES);
        assert_eq!(engine.confirmed_end_frame(), 49);
        assert_eq!(engine.hypothesis_start_frame(), 50);
    }

    #[test]
    fn disagreement_resets_evidence_for_the_changed_prefix() {
        let mut engine = WordAgreementEngine::new();
        engine.process(words(THREE_SENTENCES, 0));
        engine.process(words(
            "wrong two three four five. six seven eight. nine ten eleven.",
            0,
        ));
        let third = engine.process(words(THREE_SENTENCES, 0));
        assert!(third.newly_confirmed_text.is_empty());

        engine.process(words(THREE_SENTENCES, 0));
        let fifth = engine.process(words(THREE_SENTENCES, 0));
        assert_eq!(fifth.newly_confirmed_text, "one two three four five.");
    }

    #[test]
    fn fewer_than_five_words_never_confirm() {
        let mut engine = WordAgreementEngine::new();
        for _ in 0..4 {
            let result = engine.process(words("one. two. three.", 0));
            assert!(result.newly_confirmed_text.is_empty());
        }
    }

    #[test]
    fn two_sentences_always_remain_hypothesis() {
        let mut engine = WordAgreementEngine::new();
        for _ in 0..4 {
            let result = engine.process(words("one two three. four five six.", 0));
            assert!(result.newly_confirmed_text.is_empty());
        }
    }

    #[test]
    fn low_boundary_confidence_blocks_confirmation_when_scores_exist() {
        let mut engine = WordAgreementEngine::new();
        let mut scored = words(THREE_SENTENCES, 0);
        for word in &mut scored {
            word.confidence = Some(0.9);
        }
        scored[4].confidence = Some(0.59);
        for _ in 0..3 {
            let result = engine.process(scored.clone());
            assert!(result.newly_confirmed_text.is_empty());
        }
    }

    #[test]
    fn low_confidence_just_before_the_boundary_also_blocks_confirmation() {
        let mut engine = WordAgreementEngine::new();
        let mut scored = words(THREE_SENTENCES, 0);
        for word in &mut scored {
            word.confidence = Some(0.9);
        }
        scored[3].confidence = Some(0.59);
        for _ in 0..3 {
            assert!(
                engine
                    .process(scored.clone())
                    .newly_confirmed_text
                    .is_empty()
            );
        }
    }

    #[test]
    fn absent_confidence_scores_bypass_the_gate() {
        let mut engine = WordAgreementEngine::new();
        for _ in 0..2 {
            engine.process(words(THREE_SENTENCES, 0));
        }
        assert!(
            !engine
                .process(words(THREE_SENTENCES, 0))
                .newly_confirmed_text
                .is_empty()
        );
    }

    #[test]
    fn lookback_context_is_removed_after_a_confirmation() {
        let mut engine = WordAgreementEngine::new();
        for _ in 0..3 {
            engine.process(words(THREE_SENTENCES, 0));
        }
        let with_lookback =
            "three four five. six seven eight. nine ten eleven. twelve thirteen fourteen.";

        let result = engine.process(words(with_lookback, 20));

        assert_eq!(
            result.full_text,
            "one two three four five. six seven eight. nine ten eleven. twelve thirteen fourteen."
        );
        assert!(!result.full_text.contains("five. three"));
    }

    #[test]
    fn estimated_timestamps_never_delete_new_words_after_text_overlap() {
        let mut engine = WordAgreementEngine::new();
        let full = "one two three four five. six seven eight. nine ten eleven.";
        for _ in 0..3 {
            engine.process(estimate_word_timings(full, 0, 110));
        }

        // The lookback repeats the last two confirmed words. Its proportional
        // timing estimate places "six" before the old cut frame, even though
        // it is the first unconfirmed word and must survive.
        let lookback = "four five. six seven eight. nine ten eleven. twelve thirteen fourteen.";
        let result = engine.process(estimate_word_timings(lookback, 0, 100));

        assert!(result.hypothesis_text.starts_with("six seven eight."));
        assert!(result.full_text.contains("five. six seven"));
    }

    #[test]
    fn rewritten_lookback_is_discarded_before_the_previous_hypothesis_anchor() {
        let mut engine = WordAgreementEngine::new();
        let full = "one two three four five. six seven eight. nine ten eleven.";
        for _ in 0..3 {
            engine.process(words(full, 0));
        }

        let result = engine.process(words(
            "garbled acoustic context six seven eight. nine ten eleven. twelve thirteen.",
            20,
        ));

        assert_eq!(
            result.full_text,
            "one two three four five. six seven eight. nine ten eleven. twelve thirteen."
        );
        assert!(!result.full_text.contains("garbled"));
    }

    #[test]
    fn punctuation_and_case_do_not_break_prefix_agreement() {
        let mut engine = WordAgreementEngine::new();
        engine.process(words(THREE_SENTENCES, 0));
        engine.process(words(
            "One two three four FIVE! six seven eight? nine ten eleven;",
            0,
        ));
        let result = engine.process(words(
            "ONE two three four five; six seven eight! nine ten eleven?",
            0,
        ));
        assert_eq!(result.newly_confirmed_text, "ONE two three four five;");
    }

    #[test]
    fn an_empty_pass_preserves_the_last_hypothesis() {
        let mut engine = WordAgreementEngine::new();
        engine.process(words(THREE_SENTENCES, 0));

        assert_eq!(engine.process(Vec::new()).full_text, THREE_SENTENCES);
    }

    #[test]
    fn estimated_timings_cover_the_decode_range() {
        let words = estimate_word_timings("one two three", 100, 400);
        assert_eq!(words[0], TimedWord::estimated("one", 100, 200));
        assert_eq!(words[2], TimedWord::estimated("three", 300, 400));
    }
}
