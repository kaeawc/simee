//! Fuzzy matching for pickers (the desktop's device search): a query's terms against an item's
//! fields, scored so the closest matches rank first.
//!
//! Every term has to match some field of the item. A term matches a field whole, as its prefix,
//! at a word's start, anywhere as one run, or (in a [`Field::words`] field) letter by letter
//! with gaps: `pix9` matches `acceptance-pixel-9`. A tighter match scores higher, and an item's
//! score is the sum of its terms' best. Matching ignores case.

/// One field an item is matched on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field<'a> {
    pub text: &'a str,
    /// A term may match the field's letters with gaps between them. Off for ids (a UDID, a
    /// serial), where scattered letters would match nearly any query.
    pub scattered: bool,
}

impl<'a> Field<'a> {
    /// A name or label: matched as runs and letter by letter.
    pub fn words(text: &'a str) -> Field<'a> {
        Field { text, scattered: true }
    }

    /// An id: matched only as a run of its characters.
    pub fn id(text: &'a str) -> Field<'a> {
        Field { text, scattered: false }
    }
}

/// The query's terms: its words, lowercased.
pub fn terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

const WHOLE: u32 = 1000;
const PREFIX: u32 = 900;
const WORD: u32 = 800;
const RUN: u32 = 700;
/// A letter-by-letter match starts here and loses a point per letter skipped, down to
/// [`SCATTERED_MIN`], and gains [`WORD_START_BONUS`] for each letter at a word's start.
const SCATTERED: u32 = 500;
const SCATTERED_MIN: u32 = 100;
const WORD_START_BONUS: u32 = 10;

/// Whether position `i` of `chars` starts a word: the first character, one after a character
/// that isn't a letter or digit, or a change between letters and digits (`pixel9`'s `9`).
fn word_start(chars: &[char], i: usize) -> bool {
    let Some(prev) = i.checked_sub(1).map(|p| chars[p]) else { return true };
    let c = chars[i];
    !prev.is_alphanumeric() || (prev.is_alphabetic() && c.is_numeric()) || (prev.is_numeric() && c.is_alphabetic())
}

/// How well one lowercased `term` matches `field`, or `None` when it doesn't: whole, prefix,
/// a run at a word's start, a run anywhere, then letter by letter (closest first).
pub fn term_score(term: &str, field: Field<'_>) -> Option<u32> {
    if term.is_empty() {
        return Some(0);
    }
    let text = field.text.to_lowercase();
    if text == term {
        return Some(WHOLE);
    }
    if text.starts_with(term) {
        return Some(PREFIX);
    }
    let chars: Vec<char> = text.chars().collect();
    let want: Vec<char> = term.chars().collect();
    let runs: Vec<usize> = (0..chars.len()).filter(|&i| chars[i..].starts_with(&want)).collect();
    if runs.iter().any(|&i| word_start(&chars, i)) {
        return Some(WORD);
    }
    if !runs.is_empty() {
        return Some(RUN);
    }
    if !field.scattered {
        return None;
    }
    scattered(&chars, &want)
}

/// The best letter-by-letter match of `want` in `chars`: from each place the first letter
/// occurs, the rest taken as early as they come; the tightest (and most word-aligned) wins.
fn scattered(chars: &[char], want: &[char]) -> Option<u32> {
    let mut best = None;
    for start in (0..chars.len()).filter(|&i| chars[i] == want[0]) {
        let (mut at, mut bonus, mut last) = (start + 1, u32::from(word_start(chars, start)) * WORD_START_BONUS, start);
        let mut matched = true;
        for &w in &want[1..] {
            match (at..chars.len()).find(|&i| chars[i] == w) {
                Some(i) => {
                    bonus += u32::from(word_start(chars, i)) * WORD_START_BONUS;
                    last = i;
                    at = i + 1;
                }
                None => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            // Later starts can't find what this one didn't.
            break;
        }
        let skipped = (last + 1 - start - want.len()) as u32;
        let score = SCATTERED.saturating_sub(skipped).max(SCATTERED_MIN) + bonus;
        best = best.max(Some(score));
    }
    best
}

/// An item's score for `terms` (from [`terms`]) over its `fields`: the sum of each term's best
/// field, or `None` when some term matches none. No terms match everything, at 0.
pub fn score(terms: &[String], fields: &[Field<'_>]) -> Option<u32> {
    terms.iter().try_fold(0, |sum, term| fields.iter().filter_map(|f| term_score(term, *f)).max().map(|s| sum + s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(term: &str, text: &str) -> Option<u32> {
        term_score(term, Field::words(text))
    }

    #[test]
    fn tighter_matches_score_higher() {
        let whole = one("pixel_9", "Pixel_9").expect("whole");
        let prefix = one("pix", "pixel_9").expect("prefix");
        let word = one("pix", "acceptance-pixel-9").expect("word start");
        let run = one("ixel", "acceptance-pixel-9").expect("run");
        let scattered = one("pix9", "acceptance-pixel-9").expect("scattered");
        assert!(whole > prefix && prefix > word && word > run && run > scattered, "{whole} {prefix} {word} {run} {scattered}");
        assert_eq!(one("pix9", "pixel_fold"), None);
    }

    #[test]
    fn closer_scattered_letters_score_higher() {
        let close = one("px9", "px-9").expect("close");
        let far = one("px9", "pxxxxxxxxx-9").expect("far");
        assert!(close > far, "{close} {far}");
        // The tightest of several places the letters occur wins.
        assert_eq!(one("ab", "a---ab"), Some(WORD));
        assert!(one("ac", "a--------c a-c").expect("two starts") > one("ac", "a--------c").expect("one start"));
    }

    #[test]
    fn letters_and_digits_start_words_of_their_own() {
        assert_eq!(one("9", "pixel9"), Some(WORD));
        assert_eq!(one("17", "iPhone 17 Pro"), Some(WORD));
        assert_eq!(one("26.5", "iOS 26.5"), Some(WORD));
        assert_eq!(one("one", "iPhone"), Some(RUN));
    }

    #[test]
    fn ids_match_only_as_runs() {
        let udid = "8B529975-BB5F-4ED5-8309-0FF04764BAE6";
        assert_eq!(term_score("8b52", Field::id(udid)), Some(PREFIX));
        assert_eq!(term_score("4ed5", Field::id(udid)), Some(WORD));
        assert_eq!(term_score("8b4e", Field::id(udid)), None, "no scattered letters in an id");
        assert!(term_score("8b4e", Field::words(udid)).is_some());
    }

    #[test]
    fn every_term_must_match_some_field() {
        let fields = [Field::words("iPhone 17 Pro"), Field::words("iOS 26.5"), Field::id("A1")];
        let both = score(&terms("17 26.5"), &fields).expect("both terms");
        assert_eq!(both, WORD + WORD);
        assert_eq!(score(&terms("17 27.0"), &fields), None, "27.0 matches nothing");
        assert_eq!(score(&terms("  "), &fields), Some(0), "no terms match everything");
        assert_eq!(terms(" Pix  9 "), ["pix", "9"]);
    }
}
