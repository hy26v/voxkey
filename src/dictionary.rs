// ABOUTME: Applies user dictionary word replacements to transcription output.
// ABOUTME: Also builds vocabulary hint prompts and streaming word-boundary splits.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use voxkey_ipc::WordReplacement;

/// Remove common recognizer artifacts before dictionary replacements or text
/// injection. Dictation output remains a single line; paragraph formatting is
/// deliberately outside this pipeline.
pub fn filter_transcription_output(text: &str) -> String {
    static TAG_BLOCK: OnceLock<fancy_regex::Regex> = OnceLock::new();
    static STANDALONE_TAG: OnceLock<fancy_regex::Regex> = OnceLock::new();
    static BRACKETED: OnceLock<Vec<fancy_regex::Regex>> = OnceLock::new();
    static FILLERS: OnceLock<fancy_regex::Regex> = OnceLock::new();
    static SPACE_BEFORE_PUNCTUATION: OnceLock<fancy_regex::Regex> = OnceLock::new();

    let mut filtered = TAG_BLOCK
        .get_or_init(|| {
            fancy_regex::Regex::new(r"(?is)<([A-Za-z][A-Za-z0-9:_-]*)[^>]*>.*?</\1>")
                .expect("static tag-block regex must compile")
        })
        .replace_all(text, "")
        .into_owned();
    filtered = STANDALONE_TAG
        .get_or_init(|| {
            fancy_regex::Regex::new(r"(?is)<[^>]+>")
                .expect("static standalone-tag regex must compile")
        })
        .replace_all(&filtered, "")
        .into_owned();
    for regex in BRACKETED.get_or_init(|| {
        [r"\[[^\[\]]*\]", r"\([^()]*\)", r"\{[^{}]*\}"]
            .into_iter()
            .map(|pattern| fancy_regex::Regex::new(pattern).expect("static bracket regex"))
            .collect()
    }) {
        filtered = regex.replace_all(&filtered, "").into_owned();
    }
    filtered = FILLERS
        .get_or_init(|| {
            fancy_regex::Regex::new(
                r"(?i)(?<![\p{L}\p{M}\p{N}])(?:uh|um|hmm)(?:[,.])?(?![\p{L}\p{M}\p{N}])",
            )
            .expect("static filler regex must compile")
        })
        .replace_all(&filtered, "")
        .into_owned();
    filtered = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    SPACE_BEFORE_PUNCTUATION
        .get_or_init(|| {
            fancy_regex::Regex::new(r"\s+([,.!?;:])")
                .expect("static punctuation-spacing regex must compile")
        })
        .replace_all(&filtered, "$1")
        .trim()
        .to_string()
}

/// Apply the complete non-AI output-cleaning pipeline shared by live and final
/// transcripts.
pub fn process_transcription_output(text: &str, replacements: &[WordReplacement]) -> String {
    apply_replacements(&filter_transcription_output(text), replacements)
}

/// Clean one streaming chunk without losing the whitespace that separates it
/// from chunks injected before or after it. The batch/final pipeline remains
/// trimmed; only incremental injection needs these boundary spaces.
pub fn process_streaming_output(text: &str, replacements: &[WordReplacement]) -> String {
    let leading_space = text.chars().next().is_some_and(char::is_whitespace);
    let trailing_space = text.chars().next_back().is_some_and(char::is_whitespace);
    let core = process_transcription_output(text, replacements);
    if core.is_empty() {
        return core;
    }
    let mut output = String::with_capacity(core.len() + 2);
    if leading_space {
        output.push(' ');
    }
    output.push_str(&core);
    if trailing_space {
        output.push(' ');
    }
    output
}

/// Unicode ranges of scripts written without spaces, where word-boundary
/// matching would never fire; such rules use plain substring replacement.
const NON_SPACED_SCRIPTS: [std::ops::RangeInclusive<u32>; 13] = [
    0x0E00..=0x0E7F,   // Thai
    0x0E80..=0x0EFF,   // Lao
    0x1000..=0x109F,   // Myanmar
    0x1780..=0x17FF,   // Khmer
    0x3040..=0x309F,   // Hiragana
    0x30A0..=0x30FF,   // Katakana
    0x3400..=0x4DBF,   // CJK Unified Ideographs Extension A
    0x4E00..=0x9FFF,   // CJK Unified Ideographs
    0xA9E0..=0xA9FF,   // Myanmar Extended-B
    0xAA60..=0xAA7F,   // Myanmar Extended-A
    0xF900..=0xFAFF,   // CJK Compatibility Ideographs
    0xFF65..=0xFF9F,   // Halfwidth Katakana
    0x20000..=0x323AF, // CJK extensions and compatibility supplement
];

/// Characters that belong to a word for boundary purposes: letters, digits,
/// and the combining marks that complete them. Any script's letters count, so
/// a rule cannot fire in the middle of a word just because that word is not
/// written in ASCII.
const WORD_CHARACTER: &str = r"[\p{L}\p{M}\p{N}]";
const APOSTROPHE: &str = "['’]";

fn is_non_spaced_script(character: char) -> bool {
    NON_SPACED_SCRIPTS
        .iter()
        .any(|range| range.contains(&(character as u32)))
}

fn uses_word_boundaries(text: &str) -> bool {
    !text.chars().any(is_non_spaced_script)
}

/// Compile a rule's pattern, reusing the result for every later transcription.
///
/// Building one of these costs milliseconds, and the dictionary is applied to
/// every preview hypothesis and every streaming delta, so compiling per call
/// made a large dictionary cost more than the transcription it corrects. A
/// rule that cannot compile is remembered as unusable and reported once.
///
/// The cache holds one entry per distinct rule the user has ever configured,
/// which is bounded by the size of their dictionary.
fn compiled_rule(pattern: &str, variant: &str) -> Option<Arc<fancy_regex::Regex>> {
    type Cache = Mutex<HashMap<String, Option<Arc<fancy_regex::Regex>>>>;
    static COMPILED: OnceLock<Cache> = OnceLock::new();
    let compiled = COMPILED.get_or_init(Cache::default);

    if let Some(cached) = compiled.lock().unwrap().get(pattern) {
        return cached.clone();
    }

    // Compiled outside the lock: it is slow, and a concurrent duplicate is
    // cheaper than making every other transcription wait for it.
    let regex = match fancy_regex::Regex::new(pattern) {
        Ok(regex) => Some(Arc::new(regex)),
        Err(error) => {
            tracing::warn!("Skipping dictionary rule '{variant}': {error}");
            None
        }
    };
    compiled
        .lock()
        .unwrap()
        .insert(pattern.to_string(), regex.clone());
    regex
}

/// The alternatives a rule matches, longest first, so a specific phrase wins
/// over a shorter one that overlaps it.
fn variants(rule: &WordReplacement) -> Vec<&str> {
    let mut variants: Vec<&str> = rule
        .original
        .split(',')
        .map(str::trim)
        .filter(|variant| !variant.is_empty())
        .collect();
    variants.sort_by_key(|variant| std::cmp::Reverse(variant.chars().count()));
    variants
}

fn variant_pattern(variant: &str) -> String {
    let escaped = fancy_regex::escape(variant);
    if uses_word_boundaries(variant) {
        return format!(
            "(?<!{WORD_CHARACTER})(?<!{WORD_CHARACTER}{APOSTROPHE}){escaped}\
             (?!{WORD_CHARACTER})(?!{APOSTROPHE}{WORD_CHARACTER})"
        );
    }

    // Non-spaced scripts need substring matching, but a mixed Latin/CJK entry
    // still needs a boundary on each Latin edge or it can replace the middle
    // of a larger Latin word.
    let left_boundary = if variant
        .chars()
        .next()
        .is_some_and(|character| !is_non_spaced_script(character))
    {
        format!("(?<!{WORD_CHARACTER})(?<!{WORD_CHARACTER}{APOSTROPHE})")
    } else {
        String::new()
    };
    let right_boundary = if variant
        .chars()
        .next_back()
        .is_some_and(|character| !is_non_spaced_script(character))
    {
        format!("(?!{WORD_CHARACTER})(?!{APOSTROPHE}{WORD_CHARACTER})")
    } else {
        String::new()
    };
    format!("{left_boundary}{escaped}{right_boundary}")
}

struct ReplacementClaim<'a> {
    start: usize,
    end: usize,
    replacement: &'a str,
}

/// Apply enabled replacement rules to `text`. Longest originals first so
/// specific phrases win over shorter overlapping ones. Never fails: a rule
/// whose regex cannot compile is skipped with a warning.
///
/// Rules always match the original transcript. Replacement text is rendered
/// only after every rule has claimed its spans, so one rule cannot rewrite
/// text produced by another.
pub fn apply_replacements(text: &str, replacements: &[WordReplacement]) -> String {
    let mut enabled: Vec<&WordReplacement> = replacements.iter().filter(|r| r.enabled).collect();
    // Ranked by the longest phrase a rule can match. Using the whole
    // `original` field instead would let a rule listing several short
    // alternatives outrank a rule holding one long phrase.
    enabled.sort_by_key(|rule| {
        std::cmp::Reverse(
            variants(rule)
                .first()
                .map_or(0, |variant| variant.chars().count()),
        )
    });

    let mut claims: Vec<ReplacementClaim<'_>> = Vec::new();
    for rule in enabled {
        let variants = variants(rule);
        if variants.is_empty() {
            continue;
        }
        let alternatives = variants
            .into_iter()
            .map(variant_pattern)
            .collect::<Vec<_>>()
            .join("|");
        let pattern = format!("(?i)(?:{alternatives})");
        if let Some(regex) = compiled_rule(&pattern, &rule.original) {
            for found in regex.find_iter(text) {
                let found = match found {
                    Ok(found) => found,
                    Err(error) => {
                        tracing::warn!(
                            "Skipping dictionary matches for '{}': {error}",
                            rule.original
                        );
                        break;
                    }
                };
                let position = claims.partition_point(|claim| claim.start < found.start());
                let overlaps_earlier = position > 0 && claims[position - 1].end > found.start();
                let overlaps_later = claims
                    .get(position)
                    .is_some_and(|claim| claim.start < found.end());
                if overlaps_earlier || overlaps_later {
                    continue;
                }
                claims.insert(
                    position,
                    ReplacementClaim {
                        start: found.start(),
                        end: found.end(),
                        replacement: &rule.replacement,
                    },
                );
            }
        }
    }

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for claim in claims {
        result.push_str(&text[cursor..claim.start]);
        result.push_str(claim.replacement);
        cursor = claim.end;
    }
    result.push_str(&text[cursor..]);
    result
}

/// Punctuation/capitalization seed used even without a custom dictionary.
const STYLE_PROMPT: &str = "Hello, how are you doing? Nice to meet you.";

/// Build the strict vocabulary context accepted by HTTP batch providers.
///
/// Parakeet HTTP servers validate this prefix exactly, so punctuation/style
/// examples belong only in the Whisper-specific prompt below. Sending a style
/// prompt when the vocabulary is empty makes an otherwise valid request fail.
pub fn vocabulary_prompt(words: &[String]) -> Option<String> {
    let words: Vec<&str> = words
        .iter()
        .map(|word| word.trim())
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(format!("Important Vocabulary: {}", words.join(", ")))
}

/// Build the same prompt in the language explicitly configured for whisper.
/// Voxkey otherwise has no language selector, so auto/unknown languages retain
/// the neutral English seed rather than losing punctuation guidance entirely.
pub(crate) fn vocabulary_prompt_for_language(
    words: &[String],
    language: Option<&str>,
) -> Option<String> {
    let language = language
        .unwrap_or("en")
        .split(['-', '_'])
        .next()
        .unwrap_or("en")
        .to_ascii_lowercase();
    let style = match language.as_str() {
        "hi" => "नमस्ते, कैसे हैं आप? आपसे मिलकर अच्छा लगा।",
        "bn" => "নমস্কার, কেমন আছেন? আপনার সাথে দেখা হয়ে ভালো লাগলো।",
        "ja" => "こんにちは、お元気ですか？お会いできて嬉しいです。",
        "ko" => "안녕하세요, 잘 지내시나요? 만나서 반갑습니다.",
        "zh" => "你好，最近好吗？见到你很高兴。",
        "th" => "สวัสดีครับ/ค่ะ, สบายดีไหม? ยินดีที่ได้พบคุณ",
        "vi" => "Xin chào, bạn khỏe không? Rất vui được gặp bạn.",
        "yue" => "你好，最近點呀？見到你好開心。",
        "es" => "¡Hola, ¿cómo estás? Encantado de conocerte.",
        "fr" => "Bonjour, comment allez-vous? Ravi de vous rencontrer.",
        "de" => "Hallo, wie geht es dir? Schön dich kennenzulernen.",
        "it" => "Ciao, come stai? Piacere di conoscerti.",
        "pt" => "Olá, como você está? Prazer em conhecê-lo.",
        "ru" => "Здравствуйте, как ваши дела? Приятно познакомиться.",
        "pl" => "Cześć, jak się masz? Miło cię poznać.",
        "nl" => "Hallo, hoe gaat het? Aangenaam kennis te maken.",
        "tr" => "Merhaba, nasılsın? Tanıştığımıza memnun oldum.",
        "ar" => "مرحباً، كيف حالك؟ سعيد بلقائك.",
        "fa" => "سلام، حال شما چطور است؟ از آشنایی با شما خوشوقتم.",
        "he" => ",שלום, מה שלומך? נעים להכיר",
        "ta" => "வணக்கம், எப்படி இருக்கிறீர்கள்? உங்களை சந்தித்ததில் மகிழ்ச்சி.",
        "te" => "నమస్కారం, ఎలా ఉన్నారు? కలవడం చాలా సంతోషం.",
        "ml" => "നമസ്കാരം, സുഖമാണോ? കണ്ടതിൽ സന്തോഷം.",
        "kn" => "ನಮಸ್ಕಾರ, ಹೇಗಿದ್ದೀರಾ? ನಿಮ್ಮನ್ನು ಭೇಟಿಯಾಗಿ ಸಂತೋಷವಾಗಿದೆ.",
        "ur" => "السلام علیکم، کیسے ہیں آپ؟ آپ سے مل کر خوشی ہوئی۔",
        _ => STYLE_PROMPT,
    };
    let words: Vec<&str> = words
        .iter()
        .map(|word| word.trim())
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        Some(style.to_string())
    } else {
        Some(format!(
            "{style}\nImportant Vocabulary: {}",
            words.join(", ")
        ))
    }
}

/// Split a streaming text buffer into the part that is safe to inject now and
/// the part that must be held back for the next delta.
///
/// A trailing partial word is always held back. So is any trailing run of
/// words that reads as the beginning of a replacement rule, because the rest
/// of that rule may still arrive. Injecting it early would type the
/// uncorrected words while the saved transcript recorded the corrected ones.
pub fn split_ready<'a>(pending: &'a str, replacements: &[WordReplacement]) -> (&'a str, &'a str) {
    let safe_end = unclosed_artifact_start(pending).unwrap_or(pending.len());
    let safe = &pending[..safe_end];
    let last_whitespace = match safe.rfind(char::is_whitespace) {
        Some(index) => index,
        None if safe.chars().any(is_non_spaced_script) => {
            let boundary = safe
                .char_indices()
                .map(|(index, _)| index)
                .find(|start| begins_a_rule(&safe[*start..], replacements))
                .unwrap_or(safe.len());
            return (&pending[..boundary], &pending[boundary..]);
        }
        None => return ("", pending),
    };
    let mut boundary = last_whitespace + safe[last_whitespace..].chars().next().unwrap().len_utf8();
    boundary = protect_artifact_boundary(safe, boundary);

    for start in word_starts(safe) {
        if start >= boundary {
            break;
        }
        if begins_a_rule(&pending[start..boundary], replacements) {
            boundary = start;
            break;
        }
    }

    (&pending[..boundary], &pending[boundary..])
}

/// Do not split a complete artifact merely because it contains whitespace.
/// Once a boundary after the closing delimiter arrives, the filter can remove
/// the artifact as one unit.
fn protect_artifact_boundary(text: &str, mut boundary: usize) -> usize {
    let mut brackets: Vec<(char, usize)> = Vec::new();
    for (index, character) in text.char_indices() {
        if matches!(character, '[' | '(' | '{') {
            brackets.push((character, index));
            continue;
        }
        let expected = match character {
            ']' => Some('['),
            ')' => Some('('),
            '}' => Some('{'),
            _ => None,
        };
        let Some(expected) = expected else {
            continue;
        };
        if let Some(position) = brackets.iter().rposition(|(open, _)| *open == expected) {
            let (_, start) = brackets.remove(position);
            let end = index + character.len_utf8();
            if boundary > start && boundary < end {
                boundary = boundary.min(start);
            }
        }
    }

    static TAG_BLOCK: OnceLock<fancy_regex::Regex> = OnceLock::new();
    let tag_block = TAG_BLOCK.get_or_init(|| {
        fancy_regex::Regex::new(r"(?is)<([A-Za-z][A-Za-z0-9:_-]*)[^>]*>.*?</\1>")
            .expect("static streaming tag-block regex must compile")
    });
    for found in tag_block.find_iter(text).flatten() {
        if boundary > found.start() && boundary < found.end() {
            boundary = boundary.min(found.start());
        }
    }
    boundary
}

/// First byte that begins an artifact whose closing delimiter has not arrived
/// yet. Keeping it in the streaming buffer prevents `[music` in one delta and
/// `]` in the next from leaking as two pieces that can no longer be filtered.
fn unclosed_artifact_start(text: &str) -> Option<usize> {
    let mut earliest = None;
    for (open, close) in [('[', ']'), ('(', ')'), ('{', '}')] {
        let mut pending = Vec::new();
        for (index, character) in text.char_indices() {
            if character == open {
                pending.push(index);
            } else if character == close {
                pending.pop();
            }
        }
        if let Some(index) = pending.first().copied() {
            earliest = Some(earliest.map_or(index, |current: usize| current.min(index)));
        }
    }

    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find('<') {
        let start = cursor + relative_start;
        let Some(relative_end) = text[start..].find('>') else {
            earliest = Some(earliest.map_or(start, |current| current.min(start)));
            break;
        };
        let end = start + relative_end;
        let inside = text[start + 1..end].trim();
        if !inside.starts_with('/')
            && !inside.starts_with('!')
            && !inside.starts_with('?')
            && !inside.ends_with('/')
        {
            let name = inside
                .split(|character: char| character.is_whitespace() || character == '/')
                .next()
                .unwrap_or_default();
            if !name.is_empty() {
                let closing = format!("</{}", name.to_lowercase());
                if !text[end + 1..].to_lowercase().contains(&closing) {
                    earliest = Some(earliest.map_or(start, |current| current.min(start)));
                }
            }
        }
        cursor = end + 1;
    }
    earliest
}

fn is_word_character(character: char) -> bool {
    static WORD: OnceLock<fancy_regex::Regex> = OnceLock::new();
    let regex = WORD.get_or_init(|| {
        fancy_regex::Regex::new(&format!("^{WORD_CHARACTER}$"))
            .expect("the dictionary word-character expression is valid")
    });
    regex
        .is_match(character.encode_utf8(&mut [0; 4]))
        .unwrap_or(false)
}

/// Byte offsets where a replacement may begin according to the same Unicode
/// boundary rules used by `apply_replacements`. Punctuation is a boundary too;
/// considering whitespace alone would inject the start of a phrase such as
/// `say:vox key` before the second word arrived.
fn word_starts(text: &str) -> impl Iterator<Item = usize> {
    let mut starts = vec![0];
    let mut previous_is_word = false;
    for (index, character) in text.char_indices() {
        let is_apostrophe = matches!(character, '\'' | '’');
        let blocks_boundary = is_word_character(character) || (is_apostrophe && previous_is_word);
        if !blocks_boundary {
            starts.push(index + character.len_utf8());
        }
        previous_is_word = is_word_character(character);
    }
    starts.into_iter()
}

/// Whether `segment` reads as the start of some enabled rule without being the
/// whole of it, so a match beginning there would run past the end of the text
/// about to be injected.
fn begins_a_rule(segment: &str, replacements: &[WordReplacement]) -> bool {
    let tail = segment.to_lowercase();
    replacements
        .iter()
        .filter(|rule| rule.enabled)
        .flat_map(variants)
        .any(|variant| {
            let variant = variant.to_lowercase();
            variant != tail && variant.starts_with(&tail)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(original: &str, replacement: &str) -> WordReplacement {
        WordReplacement {
            original: original.to_string(),
            replacement: replacement.to_string(),
            enabled: true,
        }
    }

    #[test]
    fn output_filter_removes_tags_bracketed_hallucinations_and_fillers() {
        assert_eq!(
            filter_transcription_output(
                "Um, hello <noise>do not keep this</noise> [MUSIC] (applause) {cough} hmm world."
            ),
            "hello world."
        );
    }

    #[test]
    fn output_filter_respects_word_boundaries_and_cleans_spacing() {
        assert_eq!(
            filter_transcription_output("the hummingbird, um, is humming; uh yes"),
            "the hummingbird, is humming; yes"
        );
    }

    #[test]
    fn processing_filters_before_applying_dictionary_rules() {
        let rules = vec![rule("vox key", "Voxkey")];
        assert_eq!(
            process_transcription_output("um [noise] vox key", &rules),
            "Voxkey"
        );
    }

    #[test]
    fn replaces_word_case_insensitively() {
        let rules = vec![rule("jon", "John")];
        assert_eq!(
            apply_replacements("I met jon today", &rules),
            "I met John today"
        );
        assert_eq!(apply_replacements("Jon said hi", &rules), "John said hi");
    }

    #[test]
    fn respects_word_boundaries_at_punctuation() {
        let rules = vec![rule("jon", "John")];
        assert_eq!(apply_replacements("Hello, jon!", &rules), "Hello, John!");
        assert_eq!(
            apply_replacements("jonathan is here", &rules),
            "jonathan is here"
        );
    }

    #[test]
    fn apostrophes_inside_words_do_not_create_replacement_boundaries() {
        let rules = vec![rule("can", "CAN")];
        assert_eq!(
            apply_replacements("I can't and can", &rules),
            "I can't and CAN"
        );
        assert_eq!(
            apply_replacements("I can’t and can", &rules),
            "I can’t and CAN"
        );
        assert_eq!(apply_replacements("'can'", &rules), "'CAN'");
    }

    #[test]
    fn comma_separated_variants_all_map_to_replacement() {
        let rules = vec![rule("vox key, box key", "Voxkey")];
        assert_eq!(
            apply_replacements("open vox key now", &rules),
            "open Voxkey now"
        );
        assert_eq!(
            apply_replacements("open box key now", &rules),
            "open Voxkey now"
        );
    }

    #[test]
    fn alternatives_in_one_rule_do_not_rewrite_its_replacement() {
        let rules = vec![rule("foo, bar", "foo bar")];

        assert_eq!(apply_replacements("foo", &rules), "foo bar");
    }

    #[test]
    fn one_rule_does_not_rewrite_another_rules_output() {
        let rules = vec![rule("btw", "by the way"), rule("way", "Way")];
        assert_eq!(apply_replacements("btw hello", &rules), "by the way hello");

        let rules = vec![rule("東京", "京都"), rule("京都", "Kyoto")];
        assert_eq!(
            apply_replacements("私は東京にいます", &rules),
            "私は京都にいます"
        );
    }

    #[test]
    fn reciprocal_rules_exchange_words_instead_of_collapsing_them() {
        let rules = vec![rule("cat", "dog"), rule("dog", "cat")];
        assert_eq!(apply_replacements("cat and dog", &rules), "dog and cat");
    }

    #[test]
    fn longer_rules_win_over_shorter_overlapping_ones() {
        let rules = vec![rule("key", "KEY"), rule("vox key", "Voxkey")];
        assert_eq!(apply_replacements("vox key", &rules), "Voxkey");
    }

    /// Precedence follows the longest phrase a rule can actually match, not
    /// how much text the rule's list of alternatives happens to occupy.
    #[test]
    fn a_rule_with_many_short_variants_does_not_outrank_a_longer_phrase() {
        let rules = vec![rule("vox, box", "Voxkey"), rule("vox key", "Voxkey Pro")];
        assert_eq!(
            apply_replacements("open vox key now", &rules),
            "open Voxkey Pro now"
        );
        assert_eq!(
            apply_replacements("open box now", &rules),
            "open Voxkey now"
        );
    }

    #[test]
    fn disabled_rules_are_ignored() {
        let mut r = rule("jon", "John");
        r.enabled = false;
        assert_eq!(apply_replacements("jon", &[r]), "jon");
    }

    #[test]
    fn regex_special_characters_in_original_are_literal() {
        let rules = vec![rule("c++ (lang)", "C++")];
        assert_eq!(
            apply_replacements("i like c++ (lang) a lot", &rules),
            "i like C++ a lot"
        );
    }

    #[test]
    fn dollar_signs_in_replacement_are_literal() {
        let rules = vec![rule("price", "$1")];
        assert_eq!(apply_replacements("the price", &rules), "the $1");
    }

    #[test]
    fn cjk_originals_use_substring_replacement() {
        let rules = vec![rule("東京", "Tokyo")];
        assert_eq!(
            apply_replacements("私は東京にいます", &rules),
            "私はTokyoにいます"
        );
    }

    #[test]
    fn cjk_extension_characters_use_substring_replacement() {
        // U+3400 is the first character in CJK Unified Ideographs Extension A.
        let rules = vec![rule("㐀", "rare")];
        assert_eq!(apply_replacements("一㐀二", &rules), "一rare二");
    }

    #[test]
    fn halfwidth_katakana_rules_use_non_spaced_matching() {
        let rules = vec![rule("ｶﾀ", "kata")];

        assert_eq!(apply_replacements("ｱｶﾀｶﾅ", &rules), "ｱkataｶﾅ");
    }

    #[test]
    fn other_non_spaced_scripts_use_substring_replacement() {
        let lao = vec![rule("ລາວ", "Laos")];
        assert_eq!(apply_replacements("ປະເທດລາວ", &lao), "ປະເທດLaos");

        let khmer = vec![rule("ខ្មែរ", "Khmer")];
        assert_eq!(apply_replacements("ភាសាខ្មែរ", &khmer), "ភាសាKhmer");
    }

    #[test]
    fn mixed_latin_and_cjk_rules_remain_case_insensitive() {
        let rules = vec![rule("iPhone東京", "Apple Tokyo")];
        assert_eq!(
            apply_replacements("IPHONE東京 store", &rules),
            "Apple Tokyo store"
        );
    }

    #[test]
    fn mixed_script_rules_keep_boundaries_on_their_latin_edges() {
        let latin_first = vec![rule("iPhone東京", "Apple Tokyo")];
        assert_eq!(
            apply_replacements("myiPhone東京 iPhone東京", &latin_first),
            "myiPhone東京 Apple Tokyo"
        );

        let latin_last = vec![rule("東京Store", "Tokyo shop")];
        assert_eq!(
            apply_replacements("東京Storefront 東京Store", &latin_last),
            "東京Storefront Tokyo shop"
        );
    }

    #[test]
    fn hangul_rules_do_not_replace_inside_larger_korean_words() {
        let rules = vec![rule("한", "ONE")];
        assert_eq!(apply_replacements("한글 한", &rules), "한글 ONE");
    }

    #[test]
    fn accented_letters_are_part_of_the_surrounding_word() {
        let rules = vec![rule("se", "SE")];
        assert_eq!(apply_replacements("señor", &rules), "señor");
        assert_eq!(
            apply_replacements("el señor se fue", &rules),
            "el señor SE fue"
        );
    }

    #[test]
    fn non_latin_alphabets_respect_word_boundaries() {
        let rules = vec![rule("он", "HE")];
        assert_eq!(apply_replacements("она пришла", &rules), "она пришла");
        assert_eq!(apply_replacements("он пришёл", &rules), "HE пришёл");

        let greek = vec![rule("και", "AND")];
        assert_eq!(apply_replacements("καιρός", &greek), "καιρός");
        assert_eq!(apply_replacements("ένα και δύο", &greek), "ένα AND δύο");
    }

    /// A multi-byte letter immediately before a candidate match must block it.
    /// This is the case a byte-oriented boundary check gets wrong.
    #[test]
    fn a_multibyte_letter_before_the_match_blocks_it() {
        let rules = vec![rule("а", "A")];
        assert_eq!(apply_replacements("она", &rules), "она");
        assert_eq!(apply_replacements("а он", &rules), "A он");

        let latin = vec![rule("or", "OR")];
        assert_eq!(apply_replacements("señor", &latin), "señor");
    }

    #[test]
    fn non_latin_replacements_are_case_insensitive() {
        let rules = vec![rule("он", "HE")];
        assert_eq!(apply_replacements("Он ушёл", &rules), "HE ушёл");
    }

    #[test]
    fn combining_marks_do_not_split_a_word() {
        // "café" written as "cafe" plus a combining acute accent.
        let rules = vec![rule("cafe", "CAFE")];
        assert_eq!(
            apply_replacements("cafe\u{301} au lait", &rules),
            "cafe\u{301} au lait"
        );
        assert_eq!(apply_replacements("cafe au lait", &rules), "CAFE au lait");
    }

    #[test]
    fn non_ascii_digits_are_part_of_the_surrounding_word() {
        // Devanagari digit five following an ASCII word.
        let rules = vec![rule("q", "Q")];
        assert_eq!(apply_replacements("q\u{096B}", &rules), "q\u{096B}");
    }

    #[test]
    fn empty_rules_return_text_unchanged() {
        assert_eq!(apply_replacements("hello", &[]), "hello");
    }

    #[test]
    fn vocabulary_prompt_joins_words() {
        let words = vec!["Voxkey".to_string(), "Barduhn".to_string()];
        assert_eq!(
            vocabulary_prompt(&words).unwrap(),
            "Important Vocabulary: Voxkey, Barduhn"
        );
    }

    #[test]
    fn vocabulary_prompt_empty_is_none() {
        assert_eq!(vocabulary_prompt(&[]), None);
    }

    #[test]
    fn vocabulary_prompt_ignores_blank_and_padded_entries() {
        let words = vec!["  ".to_string(), " Voxkey ".to_string(), String::new()];
        assert_eq!(
            vocabulary_prompt(&words).as_deref(),
            Some("Important Vocabulary: Voxkey")
        );
        assert_eq!(vocabulary_prompt(&[" \t".to_string()]), None);
    }

    #[test]
    fn vocabulary_prompt_uses_the_configured_language_and_base_code() {
        assert_eq!(
            vocabulary_prompt_for_language(&[], Some("es-ES")).as_deref(),
            Some("¡Hola, ¿cómo estás? Encantado de conocerte.")
        );
        assert_eq!(
            vocabulary_prompt_for_language(&[], Some("de_DE")).as_deref(),
            Some("Hallo, wie geht es dir? Schön dich kennenzulernen.")
        );
        assert_eq!(
            vocabulary_prompt_for_language(&[], Some("auto")).as_deref(),
            Some(STYLE_PROMPT)
        );
    }

    #[test]
    fn split_ready_splits_at_last_whitespace() {
        assert_eq!(split_ready("hello wor", &[]), ("hello ", "wor"));
        assert_eq!(split_ready("hello world ", &[]), ("hello world ", ""));
        assert_eq!(split_ready("partial", &[]), ("", "partial"));
        assert_eq!(split_ready("", &[]), ("", ""));
    }

    /// Replay the streaming injection loop: each delta extends the buffer, the
    /// ready part is corrected and "injected", and the rest is held back.
    /// Returns what the user would see typed and what is saved as the
    /// transcript, which must agree.
    fn replay_stream(deltas: &[&str], rules: &[WordReplacement]) -> (String, String) {
        let mut pending = String::new();
        let mut injected = String::new();
        let mut accumulated = String::new();
        for delta in deltas {
            accumulated.push_str(delta);
            pending.push_str(delta);
            let (ready, rest) = split_ready(&pending, rules);
            let rest = rest.to_string();
            if !ready.is_empty() {
                injected.push_str(&process_streaming_output(ready, rules));
                pending = rest;
            }
        }
        injected.push_str(&process_streaming_output(&pending, rules));
        (injected, process_transcription_output(&accumulated, rules))
    }

    #[test]
    fn streaming_holdback_replaces_a_word_split_across_deltas() {
        let rules = vec![rule("voxky", "Voxkey")];
        let (injected, _) = replay_stream(&["open vox", "ky no", "w"], &rules);
        assert_eq!(injected, "open Voxkey now");
    }

    #[test]
    fn streaming_holdback_replaces_a_phrase_split_across_deltas() {
        let rules = vec![rule("vox key", "Voxkey")];
        let (injected, _) = replay_stream(&["open vox ", "key now"], &rules);
        assert_eq!(injected, "open Voxkey now");
    }

    #[test]
    fn streaming_holdback_finds_a_phrase_after_punctuation() {
        let rules = vec![rule("vox key", "Voxkey")];
        let (injected, recorded) = replay_stream(&["say:vox ", "key now"], &rules);

        assert_eq!(injected, "say:Voxkey now");
        assert_eq!(injected, recorded);
    }

    /// Whatever the provider's delta boundaries happen to be, the text typed
    /// into the focused window must equal the transcript Voxkey saves.
    #[test]
    fn streaming_injects_exactly_what_it_records_for_every_delta_split() {
        let rules = vec![rule("vox key, box key", "Voxkey")];
        let text = "open vox key now";

        for first in 1..text.len() {
            for second in first + 1..text.len() {
                let deltas = [&text[..first], &text[first..second], &text[second..]];
                let (injected, recorded) = replay_stream(&deltas, &rules);
                assert_eq!(
                    injected, recorded,
                    "delta split {deltas:?} typed text that differs from the saved transcript"
                );
            }
        }
    }

    #[test]
    fn streaming_filter_holds_split_artifacts_and_preserves_word_spaces() {
        let rules = vec![rule("vox key", "Voxkey")];
        let text = "um, open [background music] vox key now";

        for first in 1..text.len() {
            for second in first + 1..text.len() {
                let deltas = [&text[..first], &text[first..second], &text[second..]];
                let (injected, recorded) = replay_stream(&deltas, &rules);
                assert_eq!(injected, "open Voxkey now", "delta split {deltas:?}");
                assert_eq!(injected, recorded, "delta split {deltas:?}");
            }
        }
    }

    #[test]
    fn split_ready_keeps_an_unclosed_tag_or_bracket_for_the_next_delta() {
        assert_eq!(
            split_ready("hello [background ", &[]),
            ("hello ", "[background ")
        );
        assert_eq!(
            split_ready("hello <noise>background ", &[]),
            ("hello ", "<noise>background ")
        );
        assert_eq!(
            split_ready("hello [background music] world ", &[]),
            ("hello [background music] world ", "")
        );
    }

    #[test]
    fn split_ready_holds_back_words_that_may_still_complete_a_rule() {
        let rules = vec![rule("vox key", "Voxkey")];
        assert_eq!(split_ready("open vox ", &rules), ("open ", "vox "));
        assert_eq!(split_ready("open vox key ", &rules), ("open vox key ", ""));
    }

    #[test]
    fn split_ready_streams_non_spaced_text_without_breaking_rules() {
        let rules = vec![rule("北京", "Beijing")];

        assert_eq!(split_ready("我爱北", &rules), ("我爱", "北"));
        assert_eq!(split_ready("北京大学", &rules), ("北京大学", ""));
        assert_eq!(split_ready("你好", &[]), ("你好", ""));
    }

    #[test]
    fn split_ready_ignores_rules_that_cannot_extend_the_tail() {
        let rules = vec![rule("vox key", "Voxkey")];
        assert_eq!(split_ready("open box ", &rules), ("open box ", ""));
        assert_eq!(split_ready("a monkey ", &rules), ("a monkey ", ""));
    }

    #[test]
    fn split_ready_holds_back_regardless_of_rule_casing() {
        let rules = vec![rule("Vox Key", "Voxkey")];
        assert_eq!(split_ready("open VOX ", &rules), ("open ", "VOX "));
    }

    #[test]
    fn split_ready_ignores_disabled_rules() {
        let mut disabled = rule("vox key", "Voxkey");
        disabled.enabled = false;
        assert_eq!(split_ready("open vox ", &[disabled]), ("open vox ", ""));
    }

    /// Compiling a rule costs milliseconds and the dictionary runs on every
    /// preview and every streaming delta, so the same rule must never be
    /// compiled twice.
    #[test]
    fn a_rule_is_compiled_once_and_reused() {
        let pattern = format!("(?i)(?<!{WORD_CHARACTER})reuse-probe(?!{WORD_CHARACTER})");

        let first = compiled_rule(&pattern, "reuse-probe").unwrap();
        let second = compiled_rule(&pattern, "reuse-probe").unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "the rule was compiled again instead of being reused"
        );
    }

    #[test]
    fn a_rule_that_cannot_compile_is_skipped_and_stays_skipped() {
        assert!(compiled_rule("(?i)(unclosed", "broken").is_none());
        assert!(compiled_rule("(?i)(unclosed", "broken").is_none());
    }

    /// Applying a large dictionary happens on every preview refresh, so it has
    /// to stay far below the interval between refreshes.
    #[test]
    fn a_large_dictionary_applies_quickly_enough_for_live_previews() {
        let rules: Vec<WordReplacement> = (0..50)
            .map(|index| rule(&format!("term{index}, alt{index}"), &format!("Term{index}")))
            .collect();
        let text = "the quick brown fox jumps over the lazy dog ".repeat(12);

        // Warm the cache the way a running session would, then measure steady
        // state rather than one-off startup cost.
        apply_replacements(&text, &rules);
        let started = std::time::Instant::now();
        for _ in 0..10 {
            apply_replacements(&text, &rules);
        }
        let per_call = started.elapsed() / 10;

        assert!(
            per_call < std::time::Duration::from_millis(250),
            "applying 50 rules took {per_call:?} per call; \
             rules are being recompiled instead of reused"
        );
    }
}
