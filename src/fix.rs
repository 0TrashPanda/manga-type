//! Post-OCR word correction for comic lettering.
//!
//! Comic fonts trip Tesseract in consistent ways. On Star-Embracing Swordmaster the D has a
//! gap at its lower left and reads as P ("PIRECTOR", "ANP", "PON'T"), the U is open and
//! reads as L ("YOL", "REQLEST"), and `?` reads as `Z` or `2` ("THATZ", "RIGHT2").
//! Preprocessing the image doesn't help (tried scale, binarize, bold, psm, legacy engine),
//! so words are corrected against a dictionary instead:
//!
//! 1. A word not in the dictionary that becomes one by swapping lookalike letters is fixed.
//! 2. A short real word (PO, PIP, HAP) is swapped only when this chapter shows that
//!    confusion several times, and only toward a very common word (DO, DID, HAD).
//! 3. A trailing `Z`/`2` that makes a non-word becomes `?`; digits inside words become
//!    letters (`T0` -> `TO`); a lone `1`/`T` becomes `I`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

use crate::ocr::Bubble;

/// Lookalike pairs: (what OCR printed, what the letterer drew).
const LOOKALIKES: &[(char, char)] = &[('P', 'D'), ('L', 'U'), ('F', 'P'), ('H', 'U')];
/// Multi-letter misreads, only used to turn a non-word into a word. Seen on Absolute
/// Sword Sense: the open U reads as LI/LU/CLI (MOLINTAIN, YOLUNG, AROCLIND), and a
/// stray P appears next to D (BLOOPD, HANDPLE) or F next to P (PEOFPLE).
const SEQUENCES: &[(&str, &str)] = &[
    ("LI", "U"),
    ("LU", "U"),
    ("CLI", "U"),
    ("LL", "U"),
    ("PD", "D"),
    ("DP", "D"),
    ("FP", "P"),
];
/// Digits read inside words.
const DIGIT_LETTERS: &[(char, char)] =
    &[('0', 'O'), ('1', 'I'), ('5', 'S'), ('6', 'G'), ('7', 'T'), ('8', 'B')];
/// Non-word fixes of one kind needed in a chapter before also swapping real words.
const MIN_EVIDENCE: usize = 3;

/// Very common English words: targets for swapping a real-but-unlikely word (rule 2).
const COMMON: &str = "A ABOUT AFTER AGAIN ALL ALREADY ALSO ALWAYS AM AN AND ANY ANYTHING ARE AREN'T AROUND AS ASK AT AWAY \
BACK BAD BE BECAUSE BEEN BEFORE BEING BETTER BIG BODY BOTH BUT BY CAN CAN'T COULD COULDN'T DAD DAMN DAY DAYS DEAD DEAL \
DEATH DID DIDN'T DIE DIED DO DOES DOESN'T DOING DON'T DONE DOOR DOWN DUDE DURING EACH END EVEN EVER EVERY EYES FACE FIND \
FOR FROM GET GIVE GO GOD GOING GOOD GOT HAD HADN'T HAND HANDS HAS HAVE HE HE'D HE'S HEAD HEAR HELL HER HERE HIM HIS HOLD \
HOW HUH I I'D I'LL I'M I'VE IDEA IF IN INDEED INTO IS ISN'T IT IT'S ITS JUST KID KIDS KILL KIND KNOW LADY LAST LET LET'S \
LIKE LITTLE LONG LOOK LORD MADE MAKE MAN ME MIND MORE MOST MUCH MUST MY NEED NEVER NEW NO NOT NOTHING NOW OF OFF OH OK OKAY \
OLD ON ONE ONLY OR OTHER OUR OUT OVER OWN PEOPLE PLEASE PUT READ READY REAL REALLY RIGHT SAD SAID SAME SAY SEE SHE SHE'D \
SHOULD SHOULDN'T SIDE SO SOME SOMEONE SOMETHING STILL STOP SUCH SURE TAKE TELL THAN THAT THAT'S THE THEIR THEM THEN THERE \
THESE THEY THINK THIS THOSE TIME TO TODAY TOLD TOO TRY UP US USE USED VERY WAIT WANT WAS WASN'T WAY WE WE'D WE'LL WE'RE \
WELL WERE WHAT WHEN WHERE WHICH WHILE WHO WHY WILL WITH WON'T WORD WORDS WORLD WOULD WOULDN'T YEAH YES YET YOU YOU'D \
YOU'LL YOU'RE YOU'VE YOUR";

// ---------------------------------------------------------------------------
// Dictionary
// ---------------------------------------------------------------------------

/// Upper-cased English words from Tesseract's own `eng` word list.
pub struct Dictionary {
    words: HashSet<String>,
    common: HashSet<&'static str>,
}

impl Dictionary {
    pub fn from_words(words: impl IntoIterator<Item = String>) -> Self {
        Self {
            words: words.into_iter().map(|w| normalize(&w)).collect(),
            common: COMMON.split_whitespace().collect(),
        }
    }

    /// Load the cached word list, building it from the installed Tesseract data the first
    /// time (`combine_tessdata` + `dawg2wordlist`, both part of the tesseract package).
    pub fn load_or_build() -> Result<Self, String> {
        let cache = cache_dir().join("eng-words.txt");
        if let Ok(text) = std::fs::read_to_string(&cache) {
            return Ok(Self::from_words(text.lines().map(str::to_string)));
        }
        let tessdata = tessdata_dir()?;
        let tmp = cache_dir().join("tessdata-unpack");
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let prefix = tmp.join("eng.");
        run(Command::new("combine_tessdata").arg("-u").arg(tessdata.join("eng.traineddata")).arg(&prefix))?;
        let out = tmp.join("words.txt");
        run(Command::new("dawg2wordlist")
            .arg(tmp.join("eng.lstm-unicharset"))
            .arg(tmp.join("eng.lstm-word-dawg"))
            .arg(&out))?;
        std::fs::rename(&out, &cache).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&tmp);
        let text = std::fs::read_to_string(&cache).map_err(|e| e.to_string())?;
        Ok(Self::from_words(text.lines().map(str::to_string)))
    }

    fn contains(&self, w: &str) -> bool {
        let n = normalize(w);
        // The list spells contractions both with and without the apostrophe.
        self.words.contains(&n) || self.common.contains(n.as_str())
    }

    fn is_common(&self, w: &str) -> bool {
        self.common.contains(normalize(w).as_str())
    }
}

fn normalize(w: &str) -> String {
    w.replace('’', "'").to_uppercase()
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("manga-type")
}

/// Ask tesseract where its language data lives.
fn tessdata_dir() -> Result<PathBuf, String> {
    let out = Command::new("tesseract").arg("--list-langs").output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .next()
        .and_then(|l| l.split('"').nth(1))
        .map(PathBuf::from)
        .ok_or_else(|| "can't find tesseract's tessdata folder".into())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

// ---------------------------------------------------------------------------
// Correction
// ---------------------------------------------------------------------------

/// Apply `f` to every word (run of letters, digits, apostrophes) in `text`, keeping
/// everything between words (spaces, punctuation, hyphens) as is.
fn map_words(text: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String, f: &mut dyn FnMut(&str) -> String| {
        if !word.is_empty() {
            // Apostrophes at the ends aren't part of the word ('EM is rare; quotes are common).
            let core = word.trim_matches(['\'', '’']);
            let start = word.find(core).unwrap_or(0);
            out.push_str(&word[..start]);
            out.push_str(&f(core));
            out.push_str(&word[start + core.len()..]);
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '\'' || c == '’' {
            word.push(c);
        } else {
            flush(&mut word, &mut out, &mut f);
            out.push(c);
        }
    }
    flush(&mut word, &mut out, &mut f);
    out
}

/// Every way of swapping some of the `from` letters in `w` to `to` (at least one).
fn swaps(w: &str, from: char, to: char) -> Vec<String> {
    let chars: Vec<char> = w.chars().collect();
    let pos: Vec<usize> = (0..chars.len()).filter(|&i| chars[i].to_ascii_uppercase() == from).collect();
    if pos.is_empty() || pos.len() > 5 {
        return Vec::new();
    }
    (1..(1u32 << pos.len()))
        .map(|mask| {
            let mut c = chars.clone();
            for (bit, &i) in pos.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    c[i] = if chars[i].is_lowercase() { to.to_ascii_lowercase() } else { to };
                }
            }
            c.into_iter().collect()
        })
        .collect()
}

/// A non-word fixed by one lookalike pair, if any: returns (pair index, fixed word).
fn fix_nonword(w: &str, dict: &Dictionary) -> Option<(usize, String)> {
    if dict.contains(w) || !w.chars().any(char::is_alphabetic) {
        return None;
    }
    for (k, &(from, to)) in LOOKALIKES.iter().enumerate() {
        // Prefer a common word, else any dictionary word.
        let cands = swaps(w, from, to);
        if let Some(c) = cands.iter().find(|c| dict.is_common(c)).or_else(|| cands.iter().find(|c| dict.contains(c))) {
            return Some((k, c.clone()));
        }
    }
    // Multi-letter misreads, possibly together with one-letter ones (BLOOPD, YOLUNG).
    // The dictionary has junk entries, so require a real-length result.
    let upper = w.to_uppercase();
    for &(from, to) in SEQUENCES {
        for (i, _) in upper.match_indices(from) {
            let cand = format!("{}{}{}", &upper[..i], to, &upper[i + from.len()..]);
            if cand.chars().count() >= 4 && dict.contains(&cand) {
                return Some((usize::MAX, match_case(w, &cand)));
            }
            for (k, &(f, t)) in LOOKALIKES.iter().enumerate() {
                if let Some(c) = swaps(&cand, f, t).into_iter().find(|c| c.chars().count() >= 4 && dict.contains(c)) {
                    return Some((k, match_case(w, &c)));
                }
            }
        }
    }
    None
}

/// Upper-case `fixed` if the original was upper case (comics), else lower-case it.
fn match_case(original: &str, fixed: &str) -> String {
    if original.chars().any(char::is_lowercase) { fixed.to_lowercase() } else { fixed.to_string() }
}

/// Digits inside a word (`T0`, `S0`, `1T'S`), when the result is a word.
fn fix_digits(w: &str, dict: &Dictionary) -> Option<String> {
    let letters = w.chars().filter(|c| c.is_alphabetic()).count();
    if letters == 0 || !w.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let fixed: String = w
        .chars()
        .map(|c| DIGIT_LETTERS.iter().find(|(d, _)| *d == c).map_or(c, |(_, l)| *l))
        .collect();
    dict.contains(&fixed).then_some(fixed)
}

/// Correct every bubble's text in place. Bubbles the user edited are left alone.
/// Returns how many words were changed.
pub fn correct_bubbles(bubbles: &mut [Bubble], dict: &Dictionary) -> usize {
    // Pass 1: learn which lookalike confusions this chapter has, from non-words only.
    let mut evidence: HashMap<usize, usize> = HashMap::new();
    for b in bubbles.iter() {
        map_words(&b.text, |w| {
            if let Some((k, _)) = fix_nonword(w, dict) {
                *evidence.entry(k).or_default() += 1;
            }
            w.to_string()
        });
    }
    let active: Vec<usize> = evidence
        .iter()
        .filter(|(k, n)| **k < LOOKALIKES.len() && **n >= MIN_EVIDENCE)
        .map(|(k, _)| *k)
        .collect();

    // Pass 2: correct.
    let mut changed = 0;
    for b in bubbles.iter_mut().filter(|b| b.edited.is_none()) {
        let fixed = fix_text(&b.text, dict, &active, &mut changed);
        if fixed != b.text {
            if b.raw.is_none() {
                b.raw = Some(b.text.clone());
            }
            b.text = fixed;
        }
    }
    changed
}

fn fix_text(text: &str, dict: &Dictionary, active: &[usize], changed: &mut usize) -> String {
    let words: Vec<String> = {
        let mut v = Vec::new();
        map_words(text, |w| {
            v.push(w.to_string());
            w.to_string()
        });
        v
    };
    let n_alpha = words.iter().filter(|w| w.chars().any(char::is_alphabetic)).count();
    // Comic lettering is all caps; stray lowercase ("couLd", "You") is OCR noise that
    // strict typing would make you reproduce.
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    let upper = letters.iter().filter(|c| c.is_uppercase()).count();
    let all_caps = letters.len() >= 4 && upper * 10 >= letters.len() * 6;
    let source = if all_caps { text.to_uppercase() } else { text.to_string() };
    // `|` touching a letter is an I ("...|T WASN'T").
    let chars: Vec<char> = source.chars().collect();
    let source: String = chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let near_letter = |j: Option<usize>| j.and_then(|j| chars.get(j)).is_some_and(|c| c.is_alphabetic());
            if c == '|' && (near_letter(i.checked_sub(1)) || near_letter(Some(i + 1))) { 'I' } else { c }
        })
        .collect();
    let mut out = map_words(&source, |w| {
        let fixed = fix_word(w, dict, active);
        if fixed != w {
            *changed += 1;
        }
        fixed
    });
    out = fix_lone_i(&out, n_alpha, changed);
    // `?` read as `Z`/`2` glued to a word: "THATZ" -> "THAT?", "PROBLEM?Z" -> "PROBLEM?".
    out = fix_question_marks(&out, dict, changed);
    out
}

/// A standalone `1` / `T` / `l` / `|` between words is "I" (but not the T in "SH*T").
fn fix_lone_i(text: &str, n_alpha_words: usize, changed: &mut usize) -> String {
    if n_alpha_words < 2 {
        return text.to_string();
    }
    text.split(' ')
        .map(|tok| {
            let core = tok.trim_end_matches(['.', ',', '!', '?']);
            if matches!(core, "1" | "T" | "l" | "|") {
                *changed += 1;
                format!("I{}", &tok[core.len()..])
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn fix_word(w: &str, dict: &Dictionary, active: &[usize]) -> String {
    // "I" glued to the next word: INOW -> I NOW, ICAN'T -> I CAN'T.
    if !dict.contains(w)
        && let Some(rest) = w.strip_prefix('I')
        && rest.chars().count() >= 2
        && dict.contains(rest)
    {
        return format!("I {rest}");
    }
    // Contractions with I misread: T'LL, 1'M, T'VE.
    if let Some(rest) = w.strip_prefix(['T', '1', 'l'])
        && rest.starts_with(['\'', '’'])
        && dict.contains(&format!("I{rest}"))
    {
        return format!("I{rest}");
    }
    if let Some(f) = fix_digits(w, dict) {
        return f;
    }
    if let Some((_, f)) = fix_nonword(w, dict) {
        return f;
    }
    // A real but uncommon word in a chapter known for this confusion: PO -> DO.
    if dict.contains(w) && !dict.is_common(w) {
        for &k in active {
            let (from, to) = LOOKALIKES[k];
            if let Some(c) = swaps(w, from, to).into_iter().find(|c| dict.is_common(c)) {
                return c;
            }
        }
    }
    w.to_string()
}

fn fix_question_marks(text: &str, dict: &Dictionary, changed: &mut usize) -> String {
    let toks: Vec<&str> = text.split(' ').collect();
    let last = toks.len().saturating_sub(1);
    toks.iter()
        .enumerate()
        .map(|(i, tok)| {
            // Strip a Z/2 that sits where a ? belongs: at the end, or after ?/!.
            let core = tok.trim_end_matches(['?', '!', '.', ',']);
            let tail = &tok[core.len()..];
            if let Some(word) = core.strip_suffix(['Z', '2'])
                && !word.is_empty()
                && word.chars().all(|c| c.is_alphabetic() || c == '\'' || c == '?' || c == '!')
                && !dict.contains(core)
            {
                let word_part = word.trim_end_matches(['?', '!']);
                // Tesseract's list has many odd short words (LUA), so for a name like
                // "CAPTAIN LUAZ." require a common or longer word unless it ends the bubble.
                let solid = dict.is_common(word_part) || word_part.chars().count() >= 4 || i == last;
                if (dict.contains(word_part) && solid) || word.ends_with(['?', '!']) {
                    *changed += 1;
                    let mark = if word.ends_with(['?', '!']) { "" } else { "?" };
                    return format!("{word}{mark}{tail}");
                }
            }
            tok.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::{Bubble, Rect};

    fn dict() -> Dictionary {
        // Small stand-in for Tesseract's list, including its junk short words (PO, PIP, ANP).
        let words = "DIRECTOR DEPUTY DISCIPLINARY DECLARE REQUEST YOU AND PO PIP ANP HAP PIE DIE THAT \
                     WRONG ME HUH ACTION PROBLEM COMPANY OUR UNDERSTAND IDEA SESSION JAZZ QUIZ TO SO WITH DON'T \
                     SHOULD'VE I'LL RECORD MODEL WORDS";
        Dictionary::from_words(words.split_whitespace().map(String::from))
    }

    fn bubble(t: &str) -> Bubble {
        Bubble { rect: Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }, text: t.into(), conf: 90.0, edited: None, raw: None }
    }

    fn fix(texts: &[&str]) -> Vec<String> {
        let mut b: Vec<Bubble> = texts.iter().map(|t| bubble(t)).collect();
        correct_bubbles(&mut b, &dict());
        b.into_iter().map(|b| b.text).collect()
    }

    #[test]
    fn star_embracing_lines() {
        let out = fix(&[
            "THE TRAINING CENTER PIRECTOR ANP PEPUTY PIRECTOR ARE HERE.",
            "I NOW PECLARE THIS EMERGENCY PISCIPLINARY COMMITTEE IN SESSION.",
            "PO YOU REALLY THINK T PIP THATZ",
            "HAP NO IPEA. YOL PON'T REQLEST, CHAIRMANZ",
            "WHAT'S WRONGZ IS THERE A PROBLEM?Z",
            "T'LL GO T0 THE COMFANY. S0 WHAT? 100 BILLION",
        ]);
        assert_eq!(out[0], "THE TRAINING CENTER DIRECTOR AND DEPUTY DIRECTOR ARE HERE.");
        assert_eq!(out[1], "I NOW DECLARE THIS EMERGENCY DISCIPLINARY COMMITTEE IN SESSION.");
        assert_eq!(out[2], "DO YOU REALLY THINK I DID THAT?");
        assert_eq!(out[3], "HAD NO IDEA. YOU DON'T REQUEST, CHAIRMANZ", "CHAIRMAN isn't in the test dict");
        assert_eq!(out[4], "WHAT'S WRONG? IS THERE A PROBLEM?");
        assert_eq!(out[5], "I'LL GO TO THE COMPANY. SO WHAT? 100 BILLION");
    }

    #[test]
    fn caps_lone_i_and_glued_i() {
        let out = fix(&[
            "YOU LITTLE SH*T IF YOU WANT",
            "PIPN'T WE CREATE IT SO YOU couLp...",
            "INOW PECLARE IT. ICAN'T PON'T",
            "What do | do? I'm here.",
        ]);
        assert_eq!(out[0], "YOU LITTLE SH*T IF YOU WANT");
        assert_eq!(out[1], "DIDN'T WE CREATE IT SO YOU COULD...");
        assert_eq!(out[2], "I NOW DECLARE IT. I CAN'T DON'T");
        assert_eq!(out[3], "What do I do? I'm here.", "mixed case text keeps its case");
    }

    #[test]
    fn question_mark_needs_solid_word() {
        let d = Dictionary::from_words(["LUA", "SON"].map(String::from));
        let mut b = vec![bubble("THIS IS CAPTAIN LUAZ. GET AWAY"), bubble("OUR BOSS'S SONZ"), bubble("pID YOU WANT ME TO FOLLOW youz")];
        correct_bubbles(&mut b, &d);
        assert_eq!(b[0].text, "THIS IS CAPTAIN LUAZ. GET AWAY");
        assert_eq!(b[1].text, "OUR BOSS'S SON?");
        assert_eq!(b[2].text, "DID YOU WANT ME TO FOLLOW YOU?", "uppercased, then fixed");
    }

    #[test]
    fn absolute_sword_sense_lines() {
        let d = Dictionary::from_words(
            "MOUNTAIN ALTHOUGH AROUND SUFFERING YOUNG BLOOD HANDLE PEOPLE DEATH BURN UP".split(' ').map(String::from),
        );
        let mut b = vec![
            bubble("AFTER SLUFFERING IN HEAVENLY MOLINTAIN, ALTHOLIGH AROCLIND"),
            bubble("THE BLOOPD PARASITE. HANDPLE IT. SO MANY PEOFPLE"),
            bubble("WHY DO THEY LOOK SO YOLUNG? BURN 7O PEATH"),
        ];
        correct_bubbles(&mut b, &d);
        assert_eq!(b[0].text, "AFTER SUFFERING IN HEAVENLY MOUNTAIN, ALTHOUGH AROUND");
        assert_eq!(b[1].text, "THE BLOOD PARASITE. HANDLE IT. SO MANY PEOPLE");
        assert_eq!(b[2].text, "WHY DO THEY LOOK SO YOUNG? BURN TO DEATH");
    }

    #[test]
    fn leaves_real_words_without_evidence() {
        // One chapter-level P->D hint isn't enough to touch real words like PIE.
        assert_eq!(fix(&["I WANT A PIE. PIRECTOR!"]), ["I WANT A PIE. DIRECTOR!"]);
        assert_eq!(fix(&["JAZZ QUIZ"]), ["JAZZ QUIZ"]);
    }
}
