//! Typing one bubble: which keystrokes count, progress, and stats.
//!
//! The bubble text is first normalized to what you'd type on a keyboard: curly quotes
//! become straight ones, `…` becomes `...`, dashes become `-`, runs of spaces become one.
//! That is how strict mode "accepts keyboard equivalents". Characters a keyboard can't
//! type at all (`é`, `¥`, `®`) are skipped automatically in every mode.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Strictness {
    /// Only letters and digits count; spaces, punctuation and case are ignored.
    Lenient,
    /// Spaces and punctuation must be typed; case is ignored.
    CaseInsensitive,
    /// Exact characters (after keyboard normalization).
    #[default]
    Strict,
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

/// Typing state for one bubble.
#[derive(Debug, Clone)]
pub struct Typing {
    /// Normalized text.
    pub chars: Vec<char>,
    /// Which characters must be typed (others are passed over automatically).
    required: Vec<bool>,
    strictness: Strictness,
    /// Index of the next character to type (always on a required char, or `chars.len()`).
    pub pos: usize,
    /// When the last wrong key was pressed (for the red flash).
    pub last_error: Option<Instant>,
}

impl Typing {
    pub fn new(text: &str, strictness: Strictness) -> Self {
        let chars = normalize(text);
        let required = chars
            .iter()
            .map(|c| {
                let typeable = c.is_ascii_graphic() || *c == ' ';
                typeable && (strictness != Strictness::Lenient || c.is_ascii_alphanumeric())
            })
            .collect();
        let mut t = Self { chars, required, strictness, pos: 0, last_error: None };
        t.skip_unrequired();
        t
    }

    fn skip_unrequired(&mut self) {
        while self.pos < self.chars.len() && !self.required[self.pos] {
            self.pos += 1;
        }
    }

    fn matches(&self, typed: char, want: char) -> bool {
        match self.strictness {
            Strictness::Strict => typed == want,
            _ => typed.to_lowercase().eq(want.to_lowercase()),
        }
    }

    /// Handle one typed character. Returns `Some(true)` if correct, `Some(false)` if
    /// wrong, `None` if ignored (lenient mode ignores spaces and punctuation).
    pub fn type_char(&mut self, c: char) -> Option<bool> {
        if self.done() {
            return None;
        }
        if self.strictness == Strictness::Lenient && !c.is_alphanumeric() {
            return None;
        }
        if self.matches(c, self.chars[self.pos]) {
            self.pos += 1;
            self.skip_unrequired();
            Some(true)
        } else {
            self.last_error = Some(Instant::now());
            Some(false)
        }
    }

    /// Step back one required character (progress and scroll follow).
    pub fn backspace(&mut self) {
        let mut p = self.pos;
        while p > 0 {
            p -= 1;
            if self.required[p] {
                self.pos = p;
                return;
            }
        }
    }

    pub fn done(&self) -> bool {
        self.pos >= self.chars.len()
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub correct: u64,
    pub wrong: u64,
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

    fn key_at(&mut self, correct: bool, now: Instant) {
        if let Some(last) = self.last_key {
            self.seconds += now.duration_since(last).as_secs_f64().min(Self::MAX_GAP);
        }
        self.last_key = Some(now);
        if correct { self.correct += 1 } else { self.wrong += 1 }
    }

    /// Words per minute: 5 correct characters = 1 word.
    pub fn wpm(&self) -> f64 {
        if self.seconds < 1.0 { 0.0 } else { self.correct as f64 / 5.0 / (self.seconds / 60.0) }
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
        self.seconds += other.seconds;
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
        let mut t = Typing::new("DON’T…", Strictness::Strict);
        assert_eq!(type_all(&mut t, "don"), [Some(false), Some(false), Some(false)]);
        assert_eq!(t.pos, 0);
        type_all(&mut t, "DON'T...");
        assert!(t.done());
    }

    #[test]
    fn case_insensitive_needs_punctuation() {
        let mut t = Typing::new("HI, YOU.", Strictness::CaseInsensitive);
        type_all(&mut t, "hi");
        assert_eq!(t.type_char(' '), Some(false), "comma comes first");
        type_all(&mut t, ", you.");
        assert!(t.done());
    }

    #[test]
    fn lenient_letters_only() {
        let mut t = Typing::new("WHAT?! NO...", Strictness::Lenient);
        assert_eq!(t.type_char(' '), None);
        type_all(&mut t, "what");
        assert_eq!(t.type_char('?'), None);
        type_all(&mut t, "no");
        assert!(t.done());
        assert_eq!(t.progress(), 1.0);
    }

    #[test]
    fn backspace_and_progress() {
        let mut t = Typing::new("AB CD", Strictness::Strict);
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
        let mut t = Typing::new("¥100 CAFÉ", Strictness::Strict);
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
}
