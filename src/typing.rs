//! Typing one bubble: which keystrokes count, progress, and stats.
//!
//! The bubble text is first normalized to what you'd type on a keyboard: curly quotes
//! become straight ones, `…` becomes `...`, dashes become `-`, runs of spaces become one.
//! That is how strict mode "accepts keyboard equivalents". Characters a keyboard can't
//! type at all (`é`, `¥`, `®`) are skipped automatically in every mode.
//!
//! Case is its own setting ([`CaseMode`]): exact, ignored, or "first letter capitalized",
//! which turns ALL-CAPS lettering into sentence case and has you type that.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Strictness {
    /// Only letters and digits count; spaces and punctuation are ignored.
    Lenient,
    /// Spaces and punctuation must be typed too (after keyboard normalization).
    #[default]
    Strict,
}

/// What a wrong key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ErrorMode {
    /// A wrong key is rejected: it flashes red and you stay on the letter.
    #[default]
    Block,
    /// The wrong letter goes in (red) and the bubble only finishes once you've
    /// backspaced and fixed every red letter.
    Correct,
    /// The wrong letter goes in (red) and you can go on; each one still wrong when the
    /// bubble ends costs a word of net WPM.
    Keep,
}

impl ErrorMode {
    pub const ALL: [ErrorMode; 3] = [ErrorMode::Block, ErrorMode::Correct, ErrorMode::Keep];

    pub fn label(self) -> &'static str {
        match self {
            ErrorMode::Block => "Block wrong keys",
            ErrorMode::Correct => "Wrong letters stay, must fix them",
            ErrorMode::Keep => "Wrong letters stay, net WPM penalty",
        }
    }
}

/// How letter case is handled. Separate from [`Strictness`] so any mix works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CaseMode {
    /// Type exactly the case on the page (comic lettering: ALL CAPS).
    Sensitive,
    /// Any case is accepted.
    Insensitive,
    /// All-caps bubbles are turned into sentence case ("Where did you go? I was here.")
    /// and typed exactly like that. Bubbles that already have lowercase keep their case.
    #[default]
    FirstLetter,
}

impl CaseMode {
    pub const ALL: [CaseMode; 3] = [CaseMode::FirstLetter, CaseMode::Sensitive, CaseMode::Insensitive];

    pub fn label(self) -> &'static str {
        match self {
            CaseMode::Sensitive => "Case sensitive",
            CaseMode::Insensitive => "Case insensitive",
            CaseMode::FirstLetter => "First letter capitalized",
        }
    }
}

/// What the user types for a bubble's text.
pub fn normalize(text: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '’' | '‘' | '`' | '´' => out.push('\''),
            '“' | '”' | '«' | '»' => out.push('"'),
            '…' => out.extend(['.', '.', '.']),
            '—' | '–' | '‐' | '‑' => out.push('-'),
            c if c.is_whitespace() => {
                if out.last().is_some_and(|l| *l != ' ') {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }
    while out.last() == Some(&' ') {
        out.pop();
    }
    out
}

/// What the user types for a bubble's text in the given case mode.
pub fn target(text: &str, case: CaseMode) -> Vec<char> {
    let chars = normalize(text);
    if case == CaseMode::FirstLetter && !chars.iter().any(|c| c.is_lowercase()) {
        sentence_case(&chars)
    } else {
        chars
    }
}

/// Lower-case everything, then capitalize the first letter of each sentence (after `.`,
/// `!` or `?` and a space, but not after `...`, which usually trails into the same
/// sentence) and the word "I" with its contractions (I'M, I'LL, I'D, I'VE).
/// Changes letters one for one, so positions in the text stay the same.
fn sentence_case(chars: &[char]) -> Vec<char> {
    // One char in, one char out (keeps positions); leave the rare multi-char cases alone.
    fn single(mut it: impl Iterator<Item = char>, c: char) -> char {
        match (it.next(), it.next()) {
            (Some(x), None) => x,
            _ => c,
        }
    }
    let mut out: Vec<char> = chars.iter().map(|&c| single(c.to_lowercase(), c)).collect();
    let mut cap_next = true;
    let mut seen_letter = false;
    for i in 0..out.len() {
        let c = out[i];
        if c.is_alphanumeric() {
            if cap_next {
                out[i] = single(c.to_uppercase(), c);
                cap_next = false;
            }
            seen_letter = true;
        } else if matches!(c, '.' | '!' | '?') {
            let ellipsis = c == '.' && i >= 2 && out[i - 1] == '.' && out[i - 2] == '.';
            // `?!` / `!?` / `!!` end a sentence; `...` mid-bubble doesn't ("Wait... what?"),
            // but a bubble that opens with `...` still starts with a capital.
            cap_next = !ellipsis || !seen_letter;
        } else if c == ' ' || c == '"' || c == '\'' || c == '(' || c == '-' {
            // Keep waiting for the next letter.
        } else {
            cap_next = false;
        }
        // A digit or letter right after `.` (e.g. "3.5", "U.S.") isn't a new sentence.
        if seen_letter && matches!(c, '.' | '!' | '?') && out.get(i + 1).is_some_and(|n| n.is_alphanumeric()) {
            cap_next = false;
        }
    }
    // The pronoun I, alone or as I'm / I'll / I'd / I've.
    for i in 0..out.len() {
        if out[i] != 'i' {
            continue;
        }
        let before_ok = i == 0 || !out[i - 1].is_alphanumeric() && out[i - 1] != '\'';
        let after = out.get(i + 1).copied();
        let after_ok = match after {
            None => true,
            Some('\'') => {
                let rest: String = out[i + 2..].iter().take_while(|c| c.is_alphabetic()).collect();
                matches!(rest.as_str(), "m" | "ll" | "d" | "ve")
            }
            Some(n) => !n.is_alphanumeric(),
        };
        if before_ok && after_ok {
            out[i] = 'I';
        }
    }
    out
}

/// Typing state for one bubble.
#[derive(Debug, Clone)]
pub struct Typing {
    /// Normalized text.
    pub chars: Vec<char>,
    /// Which characters must be typed (others are passed over automatically).
    required: Vec<bool>,
    strictness: Strictness,
    case: CaseMode,
    errors: ErrorMode,
    /// Positions typed wrong and not fixed yet (only in [`ErrorMode::Correct`] / `Keep`).
    wrong: Vec<bool>,
    /// Index of the next character to type (always on a required char, or `chars.len()`).
    pub pos: usize,
    /// When the last wrong key was pressed (for the red flash).
    pub last_error: Option<Instant>,
}

impl Typing {
    pub fn new(text: &str, strictness: Strictness, case: CaseMode) -> Self {
        let chars = target(text, case);
        let required = chars
            .iter()
            .map(|c| {
                let typeable = c.is_ascii_graphic() || *c == ' ';
                typeable && (strictness != Strictness::Lenient || c.is_ascii_alphanumeric())
            })
            .collect();
        let wrong = vec![false; chars.len()];
        let mut t = Self { chars, required, strictness, case, errors: ErrorMode::Block, wrong, pos: 0, last_error: None };
        t.skip_unrequired();
        t
    }

    /// Set what a wrong key does (default [`ErrorMode::Block`]).
    pub fn errors(mut self, mode: ErrorMode) -> Self {
        self.errors = mode;
        self
    }

    /// Whether `c` is the right next key.
    pub fn expects(&self, c: char) -> bool {
        self.chars.get(self.pos).is_some_and(|&want| self.matches(c, want))
    }

    /// Whether the character at `i` was typed wrong and not fixed.
    pub fn is_wrong(&self, i: usize) -> bool {
        self.wrong.get(i).copied().unwrap_or(false)
    }

    /// Letters typed wrong and not fixed.
    pub fn uncorrected(&self) -> u64 {
        self.wrong.iter().filter(|w| **w).count() as u64
    }

    /// Everything is typed but red letters must be fixed first ([`ErrorMode::Correct`]).
    pub fn needs_fixing(&self) -> bool {
        self.errors == ErrorMode::Correct && self.pos >= self.chars.len() && self.uncorrected() > 0
    }

    fn skip_unrequired(&mut self) {
        while self.pos < self.chars.len() && !self.required[self.pos] {
            self.pos += 1;
        }
    }

    fn matches(&self, typed: char, want: char) -> bool {
        match self.case {
            CaseMode::Insensitive => typed.to_lowercase().eq(want.to_lowercase()),
            CaseMode::Sensitive | CaseMode::FirstLetter => typed == want,
        }
    }

    /// Switch settings mid-bubble, keeping what's typed so far.
    pub fn with_settings(&self, text: &str, strictness: Strictness, case: CaseMode, errors: ErrorMode) -> Self {
        let mut t = Self::new(text, strictness, case).errors(errors);
        if t.chars.len() == self.chars.len() {
            t.pos = self.pos;
            if errors != ErrorMode::Block {
                t.wrong = self.wrong.clone();
            }
            t.skip_unrequired();
        }
        t
    }

    /// Handle one typed character. Returns `Some(true)` if correct, `Some(false)` if
    /// wrong, `None` if ignored (lenient mode ignores spaces and punctuation).
    pub fn type_char(&mut self, c: char) -> Option<bool> {
        if self.pos >= self.chars.len() {
            // Done, or (must-fix mode) waiting for Backspace: extra keys don't count.
            if self.needs_fixing() {
                self.last_error = Some(Instant::now());
            }
            return None;
        }
        if self.strictness == Strictness::Lenient && !c.is_alphanumeric() {
            return None;
        }
        let ok = self.matches(c, self.chars[self.pos]);
        if !ok {
            self.last_error = Some(Instant::now());
            if self.errors == ErrorMode::Block {
                return Some(false);
            }
        }
        self.wrong[self.pos] = !ok;
        self.pos += 1;
        self.skip_unrequired();
        Some(ok)
    }

    /// Step back one required character (progress and scroll follow).
    pub fn backspace(&mut self) {
        let mut p = self.pos;
        while p > 0 {
            p -= 1;
            if self.required[p] {
                self.pos = p;
                self.wrong[p] = false;
                return;
            }
        }
    }

    pub fn done(&self) -> bool {
        self.pos >= self.chars.len() && !self.needs_fixing()
    }

    fn required_before(&self, pos: usize) -> usize {
        self.required[..pos.min(self.chars.len())].iter().filter(|r| **r).count()
    }

    /// Typed fraction of the required characters, 0..=1 (drives scrolling).
    pub fn progress(&self) -> f32 {
        let total = self.required_before(self.chars.len());
        if total == 0 { 1.0 } else { self.required_before(self.pos) as f32 / total as f32 }
    }

    /// Whether the last wrong key was within `flash`.
    pub fn flashing(&self, flash: Duration) -> bool {
        self.last_error.is_some_and(|t| t.elapsed() < flash)
    }
}

/// WPM and accuracy over a stretch of typing (a bubble or a chapter).
/// Time counts only while typing: gaps longer than [`Stats::MAX_GAP`] between keys
/// (reading the page, getting a drink) are capped.
/// Right and wrong presses for one expected character.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyCount {
    pub hit: u32,
    pub miss: u32,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub correct: u64,
    pub wrong: u64,
    /// Per expected character (as shown in the bubble's typing target).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<char, KeyCount>,
    /// Wrong presses as "expected→typed" ("e→r"), with counts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub typos: BTreeMap<String, u32>,
    /// Wrong letters left in when a bubble ended ([`ErrorMode::Keep`]); each costs a word.
    #[serde(default)]
    pub uncorrected: u64,
    /// Words in the bubbles typed to the end (from 2026-11-25; older stats only have
    /// characters, see [`Stats::word_count`]).
    #[serde(default)]
    pub words: u64,
    /// Active typing time in seconds.
    pub seconds: f64,
    #[serde(skip)]
    last_key: Option<Instant>,
}

impl Stats {
    pub const MAX_GAP: f64 = 3.0;

    pub fn key(&mut self, correct: bool) {
        self.key_at(correct, Instant::now());
    }

    /// A press while `expected` was the next character; `typed` is what was pressed.
    pub fn key_char(&mut self, expected: char, typed: char, correct: bool) {
        self.key(correct);
        let k = self.keys.entry(expected).or_default();
        if correct {
            k.hit += 1;
        } else {
            k.miss += 1;
            *self.typos.entry(format!("{expected}→{typed}")).or_default() += 1;
        }
    }

    fn key_at(&mut self, correct: bool, now: Instant) {
        if let Some(last) = self.last_key {
            self.seconds += now.duration_since(last).as_secs_f64().min(Self::MAX_GAP);
        }
        self.last_key = Some(now);
        if correct { self.correct += 1 } else { self.wrong += 1 }
    }

    /// Words typed, and whether that's an estimate (older stats without word counts:
    /// 5 characters = 1 word).
    pub fn word_count(&self) -> (u64, bool) {
        if self.words == 0 && self.correct > 0 { (self.correct / 5, true) } else { (self.words, false) }
    }

    /// Net words per minute: 5 correct characters = 1 word, minus one word for every
    /// wrong letter left in.
    pub fn wpm(&self) -> f64 {
        if self.seconds < 1.0 {
            0.0
        } else {
            ((self.correct as f64 / 5.0 - self.uncorrected as f64) / (self.seconds / 60.0)).max(0.0)
        }
    }

    /// Correct keystrokes / all keystrokes, in percent.
    pub fn accuracy(&self) -> f64 {
        let total = self.correct + self.wrong;
        if total == 0 { 100.0 } else { 100.0 * self.correct as f64 / total as f64 }
    }

    /// Add another stretch (a finished bubble) into these totals.
    pub fn merge(&mut self, other: &Stats) {
        self.correct += other.correct;
        self.wrong += other.wrong;
        self.uncorrected += other.uncorrected;
        self.words += other.words;
        self.seconds += other.seconds;
        for (c, k) in &other.keys {
            let e = self.keys.entry(*c).or_default();
            e.hit += k.hit;
            e.miss += k.miss;
        }
        for (t, n) in &other.typos {
            *self.typos.entry(t.clone()).or_default() += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_all(t: &mut Typing, s: &str) -> Vec<Option<bool>> {
        s.chars().map(|c| t.type_char(c)).collect()
    }

    #[test]
    fn normalizes_keyboard_equivalents() {
        let s: String = normalize("I DON’T  KNOW… “WAIT”— OK ").into_iter().collect();
        assert_eq!(s, "I DON'T KNOW... \"WAIT\"- OK");
    }

    #[test]
    fn strict_exact_with_equivalents() {
        let mut t = Typing::new("DON’T…", Strictness::Strict, CaseMode::Sensitive);
        assert_eq!(type_all(&mut t, "don"), [Some(false), Some(false), Some(false)]);
        assert_eq!(t.pos, 0);
        type_all(&mut t, "DON'T...");
        assert!(t.done());
    }

    #[test]
    fn case_insensitive_needs_punctuation() {
        let mut t = Typing::new("HI, YOU.", Strictness::Strict, CaseMode::Insensitive);
        type_all(&mut t, "hi");
        assert_eq!(t.type_char(' '), Some(false), "comma comes first");
        type_all(&mut t, ", yOU.");
        assert!(t.done());
    }

    #[test]
    fn case_sensitive_keeps_caps() {
        let mut t = Typing::new("HI YOU", Strictness::Strict, CaseMode::Sensitive);
        assert_eq!(t.type_char('H'), Some(true));
        assert_eq!(t.type_char('i'), Some(false));
    }

    fn sc(s: &str) -> String {
        target(s, CaseMode::FirstLetter).into_iter().collect()
    }

    #[test]
    fn first_letter_sentence_case() {
        assert_eq!(sc("WHERE DID YOU GO? I WAS WAITING."), "Where did you go? I was waiting.");
        assert_eq!(sc("WAIT... WHAT?! NO WAY!"), "Wait... what?! No way!");
        assert_eq!(sc("...HUH? I'M FINE, I'LL GO. IT'S 3.5 KM."), "...Huh? I'm fine, I'll go. It's 3.5 km.");
        assert_eq!(sc("\"HEY.\" (OKAY) - IS IT? HI"), "\"Hey.\" (Okay) - is it? Hi");
        assert_eq!(sc("Already mixed Case."), "Already mixed Case.", "lowercase present: untouched");
    }

    #[test]
    fn first_letter_typed_exactly() {
        let mut t = Typing::new("NO. YES", Strictness::Strict, CaseMode::FirstLetter);
        assert_eq!(t.type_char('n'), Some(false));
        type_all(&mut t, "No. Yes");
        assert!(t.done());
    }

    #[test]
    fn switching_mode_keeps_progress() {
        let mut t = Typing::new("HELLO THERE", Strictness::Strict, CaseMode::Sensitive);
        type_all(&mut t, "HELLO ");
        let t2 = t.with_settings("HELLO THERE", Strictness::Strict, CaseMode::FirstLetter, ErrorMode::Block);
        assert_eq!(t2.pos, 6);
        assert_eq!(t2.chars.iter().collect::<String>(), "Hello there");
    }

    #[test]
    fn lenient_letters_only() {
        let mut t = Typing::new("WHAT?! NO...", Strictness::Lenient, CaseMode::Insensitive);
        assert_eq!(t.type_char(' '), None);
        type_all(&mut t, "what");
        assert_eq!(t.type_char('?'), None);
        type_all(&mut t, "no");
        assert!(t.done());
        assert_eq!(t.progress(), 1.0);
    }

    #[test]
    fn backspace_and_progress() {
        let mut t = Typing::new("AB CD", Strictness::Strict, CaseMode::Sensitive);
        type_all(&mut t, "AB ");
        assert_eq!(t.progress(), 0.6);
        t.backspace();
        assert_eq!(t.pos, 2);
        t.backspace();
        t.backspace();
        t.backspace();
        assert_eq!(t.pos, 0, "can't go before the start");
    }

    #[test]
    fn untypeable_chars_are_skipped() {
        let mut t = Typing::new("¥100 CAFÉ", Strictness::Strict, CaseMode::Sensitive);
        type_all(&mut t, "100 CAF");
        assert!(t.done(), "the É is passed over: {:?}", t.pos);
    }

    #[test]
    fn stats_cap_idle_gaps() {
        let mut s = Stats::default();
        let t0 = Instant::now();
        for i in 0..10 {
            s.key_at(true, t0 + Duration::from_millis(200 * i));
        }
        s.key_at(false, t0 + Duration::from_secs(60)); // a long pause counts as 3 s
        assert!((s.seconds - (1.8 + 3.0)).abs() < 1e-6);
        assert!((s.accuracy() - 100.0 * 10.0 / 11.0).abs() < 1e-9);
        assert!((s.wpm() - 10.0 / 5.0 / (4.8 / 60.0)).abs() < 1e-6);
    }

    #[test]
    fn must_fix_mode() {
        let mut t = Typing::new("AB", Strictness::Strict, CaseMode::Sensitive).errors(ErrorMode::Correct);
        assert_eq!(type_all(&mut t, "AX"), [Some(true), Some(false)]);
        assert!(t.is_wrong(1) && !t.done() && t.needs_fixing());
        assert_eq!(t.type_char('B'), None, "no typing past the end");
        t.backspace();
        assert_eq!(t.type_char('B'), Some(true));
        assert!(t.done() && t.uncorrected() == 0);
    }

    #[test]
    fn keep_going_mode_counts_leftovers() {
        let mut t = Typing::new("ABC", Strictness::Strict, CaseMode::Sensitive).errors(ErrorMode::Keep);
        type_all(&mut t, "AXC");
        assert!(t.done());
        assert_eq!(t.uncorrected(), 1);
        let mut s = Stats { correct: 50, uncorrected: 2, seconds: 60.0, ..Default::default() };
        assert!((s.wpm() - 8.0).abs() < 1e-9, "10 words - 2");
        s.uncorrected = 20;
        assert_eq!(s.wpm(), 0.0);
    }

    #[test]
    fn block_mode_is_unchanged() {
        let mut t = Typing::new("AB", Strictness::Strict, CaseMode::Sensitive);
        assert_eq!(t.type_char('X'), Some(false));
        assert_eq!(t.pos, 0);
        assert_eq!(t.uncorrected(), 0);
    }

    #[test]
    fn old_stats_files_load() {
        let s: Stats = serde_json::from_str(r#"{"correct":3,"wrong":1,"seconds":2.0}"#).unwrap();
        assert_eq!(s.uncorrected, 0);
    }


    #[test]
    fn per_key_counts_merge() {
        let mut a = Stats::default();
        a.key_char('e', 'e', true);
        a.key_char('e', 'r', false);
        let mut b = Stats::default();
        b.key_char('e', 'r', false);
        b.key_char(' ', ' ', true);
        a.merge(&b);
        assert_eq!(a.keys[&'e'], KeyCount { hit: 1, miss: 2 });
        assert_eq!(a.typos["e→r"], 2);
        let json = serde_json::to_string(&a).unwrap();
        let back: Stats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.keys[&' '], KeyCount { hit: 1, miss: 0 });
    }

}
