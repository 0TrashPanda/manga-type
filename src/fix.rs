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
    /// Words the list only has capitalized ("Cando", "Andis"): names, so a glued
    /// "CANDO" is more likely CAN DO.
    names: HashSet<String>,
}

impl Dictionary {
    pub fn from_words(words: impl IntoIterator<Item = String>) -> Self {
        let mut all = HashSet::new();
        let mut capitalized = HashSet::new();
        let mut lower = HashSet::new();
        for w in words {
            let n = normalize(&w);
            let mut cs = w.chars();
            if cs.next().is_some_and(char::is_uppercase) && cs.any(char::is_lowercase) {
                capitalized.insert(n.clone());
            } else if w.chars().any(char::is_lowercase) {
                lower.insert(n.clone());
            }
            all.insert(n);
        }
        Self {
            words: all,
            common: COMMON.split_whitespace().collect(),
            names: capitalized.difference(&lower).cloned().collect(),
        }
    }

    /// Only known as a name (see `names`), and not a common word.
    fn is_name_only(&self, w: &str) -> bool {
        let n = normalize(w);
        self.names.contains(&n) && !self.common.contains(n.as_str())
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
    out = fix_tokens(&out, dict, changed);
    trim_stray_marks(&out)
}

/// `(leading marks, core, trailing marks)` of a space-separated token. The core keeps
/// letters, digits, apostrophes and `#` (Tesseract's H in some fonts).
fn token_parts(t: &str) -> (&str, &str, &str) {
    let keep = |c: char| c.is_alphanumeric() || c == '#';
    let Some(start) = t.find(keep) else { return (t, "", "") };
    let end = t.rfind(keep).map_or(t.len(), |i| i + t[i..].chars().next().map_or(1, char::len_utf8));
    (&t[..start], &t[start..end], &t[end..])
}

/// One letter removed from a tripled letter, or a doubled one at the start or end of the
/// word, when that makes a word: "I'LLL" -> "I'LL", "YYOU" -> "YOU", "MYY" -> "MY".
/// Doubles inside a word are left alone: real words missing from the list ("COOPED").
fn undouble(core: &str, dict: &Dictionary) -> Option<String> {
    // Runs of 3+ become 2 first ("I'LLL" -> "I'LL"); a long run is never a word anyway
    // ("AAAAAAAH" is a sound effect: no blow-up trying every removal).
    let mut chars: Vec<char> = Vec::with_capacity(core.len());
    for c in core.chars() {
        let n = chars.len();
        if n >= 2 && chars[n - 1] == c && chars[n - 2] == c {
            continue;
        }
        chars.push(c);
    }
    let collapsed: String = chars.iter().collect();
    if collapsed != core && dict.contains(&collapsed) {
        return Some(collapsed);
    }
    // Then one letter off one doubled pair: at the start or end of the word, or inside
    // it when that gives a very common word ("JUUST").
    let n = chars.len();
    for i in 1..n {
        if chars[i] != chars[i - 1] || !chars[i].is_alphabetic() {
            continue;
        }
        let cand: String = chars[..i].iter().chain(&chars[i + 1..]).collect();
        let at_edge = i == 1 || i == n - 1;
        if (at_edge || dict.is_common(&cand)) && dict.contains(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Two words glued together ("HOWPATHETIC", "CANDO", "TOSHINE"): split where both halves
/// are words and at least one is common. Prefers the split with the longest shorter half.
fn split_glued(core: &str, dict: &Dictionary) -> Option<(String, String)> {
    let chars: Vec<char> = core.chars().collect();
    if chars.len() < 4 || !chars.iter().all(|c| c.is_alphabetic()) {
        return None;
    }
    (1..chars.len())
        .filter_map(|i| {
            let (a, b): (String, String) = (chars[..i].iter().collect(), chars[i..].iter().collect());
            let short_ok = |w: &str| w.chars().count() >= 2 || matches!(w, "A" | "I" | "a");
            (short_ok(&a)
                && short_ok(&b)
                && dict.contains(&a)
                && dict.contains(&b)
                && (dict.is_common(&a) || dict.is_common(&b)))
            .then(|| (a.chars().count().min(b.chars().count()), a, b))
        })
        .max_by_key(|(k, _, _)| *k)
        .map(|(_, a, b)| (a, b))
}

/// Token-level fixes with the dictionary, after the per-word ones:
/// - `#` read for H ("T#OUG#T" -> "THOUGHT"), doubled letters ("YYOU"), glued words;
/// - "70" / "50" between words are TO / SO ("WANT 70 DIE");
/// - a word split in two by a lost hyphen ("EY ESIGHT" -> "EYESIGHT");
/// - lone letters other than A / I / O, and lone digits at the ends, are outline specks.
fn fix_tokens(text: &str, dict: &Dictionary, changed: &mut usize) -> String {
    let mut toks: Vec<String> = text.split(' ').filter(|t| !t.is_empty()).map(String::from).collect();
    let alpha = |t: &str| {
        let (_, c, _) = token_parts(t);
        !c.is_empty() && c.chars().all(|ch| ch.is_alphabetic() || ch == '\'' || ch == '’')
    };

    // Per token.
    let mut out: Vec<String> = Vec::with_capacity(toks.len());
    for i in 0..toks.len() {
        let (lead, core, trail) = token_parts(&toks[i]);
        let (lead, core, trail) = (lead.to_string(), core.to_string(), trail.to_string());
        let mut fixed: Option<String> = None;
        if core.contains('#') {
            let cand = core.replace('#', "H");
            if dict.contains(&cand) {
                fixed = Some(cand);
            }
        } else if (core == "70" || core == "50")
            && i > 0
            && i + 1 < toks.len()
            && alpha(&toks[i - 1])
            && alpha(&toks[i + 1])
            && !toks[i - 1].ends_with(['.', '!', '?'])
        {
            fixed = Some(if core == "70" { "TO" } else { "SO" }.to_string());
        } else if !core.is_empty() && !dict.contains(&core) && core.chars().any(char::is_alphabetic) {
            if let Some(c) = undouble(&core, dict) {
                fixed = Some(c);
            } else if let Some((a, b)) = split_glued(&core, dict) {
                fixed = Some(format!("{a} {b}"));
            }
        } else if dict.is_name_only(&core)
            && let Some((a, b)) = split_glued(&core, dict)
            && dict.is_common(&a)
            && dict.is_common(&b)
        {
            // "CANDO" / "ANDIS" are names in the list, but in a bubble they're CAN DO / AND IS.
            fixed = Some(format!("{a} {b}"));
        }
        match fixed {
            Some(f) => {
                *changed += 1;
                out.push(format!("{lead}{}{trail}", super::fix::match_case(&core, &f)));
            }
            None => out.push(toks[i].clone()),
        }
    }
    toks = out;

    // Join halves of a word: neither half is a word, together they are.
    let mut joined: Vec<String> = Vec::with_capacity(toks.len());
    for t in toks {
        if let Some(prev) = joined.last() {
            let (pl, pc, pt) = token_parts(prev);
            let (tl, tc, _) = token_parts(&t);
            let cand = format!("{pc}{tc}");
            if pt.is_empty()
                && tl.is_empty()
                && pc.chars().count() >= 2
                && tc.chars().count() >= 2
                && pc.chars().all(char::is_alphabetic)
                && tc.chars().all(char::is_alphabetic)
                && !dict.contains(pc)
                && !dict.contains(tc)
                && dict.contains(&cand)
            {
                *changed += 1;
                let merged = format!("{pl}{cand}{}", &t[tl.len() + tc.len()..]);
                *joined.last_mut().unwrap() = merged;
                continue;
            }
        }
        joined.push(t);
    }

    // Lone specks: single letters other than A / I / O anywhere, single digits at the ends.
    let words = joined.iter().filter(|t| token_parts(t).1.chars().filter(|c| c.is_alphabetic()).count() >= 2).count();
    if words >= 2 {
        let n = joined.len();
        let kept: Vec<String> = joined
            .iter()
            .enumerate()
            .filter(|(i, t)| {
                let speck = |t: &str| {
                    t.chars().count() == 1
                        && t.chars().all(|c| c.is_alphabetic() && !matches!(c.to_ascii_uppercase(), 'A' | 'I' | 'O'))
                };
                let digit = t.chars().count() == 1 && t.chars().all(|c| c.is_ascii_digit());
                // A digit at the end is a speck; at the start only before another speck
                // ("4 P HYUNG!"), since "5 GOLD?" is real.
                let end_digit = digit && (*i + 1 == n || (*i == 0 && joined.get(1).is_some_and(|n| speck(n))));
                !(speck(t) || end_digit)
            })
            .map(|(_, t)| t.clone())
            .collect();
        if kept.len() != joined.len() {
            *changed += 1;
            joined = kept;
        }
    }
    joined.join(" ")
}

/// Drop a lone mark token at the start or end of a bubble (". AND IS...", "RUMORS? )"):
/// specks and panel edges that OCR read as punctuation. Keeps `...` and `..`.
fn trim_stray_marks(text: &str) -> String {
    let stray = |t: &str| {
        !t.is_empty() && t.chars().count() <= 2 && t != ".." && t.chars().all(|c| ".,-_~:;()[]{}|'\"`".contains(c))
    };
    let mut toks: Vec<&str> = text.split(' ').filter(|t| !t.is_empty()).collect();
    while toks.len() > 1 && stray(toks[0]) {
        toks.remove(0);
    }
    while toks.len() > 1 && toks.last().is_some_and(|t| stray(t)) {
        toks.pop();
    }
    toks.join(" ")
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
            // "LIFE...7": a 7 right after an ellipsis is a ?.
            if let Some(head) = tok.strip_suffix('7')
                && head.ends_with("...")
                && head.chars().any(char::is_alphabetic)
            {
                *changed += 1;
                return format!("{head}?");
            }
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

    #[test]
    fn stray_marks_at_bubble_edges() {
        assert_eq!(trim_stray_marks(". AND IS INSTEAD STUCK"), "AND IS INSTEAD STUCK");
        assert_eq!(trim_stray_marks("WHAT RUMORS? )"), "WHAT RUMORS?");
        assert_eq!(trim_stray_marks("... HUH?"), "... HUH?");
        assert_eq!(trim_stray_marks("WAIT, - NO"), "WAIT, - NO", "only the edges");
        assert_eq!(trim_stray_marks("."), ".", "never empties a bubble");
    }


    #[test]
    fn seven_after_ellipsis_is_a_question_mark() {
        let mut n = 0;
        assert_eq!(fix_question_marks("YOUR PAST LIFE...7", &dict(), &mut n), "YOUR PAST LIFE...?");
        assert_eq!(fix_question_marks("CHAPTER 7", &dict(), &mut n), "CHAPTER 7");
        assert_eq!(fix_question_marks("...7", &dict(), &mut n), "...7");
    }


    #[test]
    fn token_fixes() {
        let words = "HOW PATHETIC CAN DO IT TO DIE WANT SHINE THOUGHT I'LL HELP YOU EYESIGHT VERY BAD \
                     SO TIRED I'M THE ABLE SURVIVE IF YOU SAID GOING FIGHT WHAT SHOULD WE NOW ON HAVE";
        let d = Dictionary::from_words(words.split_whitespace().map(String::from));
        let f = |t: &str| fix_tokens(t, &d, &mut 0);
        assert_eq!(f("HOWPATHETIC FOR"), "HOW PATHETIC FOR");
        assert_eq!(f("YOU CANDO IT"), "YOU CAN DO IT");
        assert_eq!(f("I T#OUG#T YOU SAID"), "I THOUGHT YOU SAID");
        assert_eq!(f("I'LLL HELP YOU."), "I'LL HELP YOU.");
        assert_eq!(f("YYOU!! DIE!"), "YOU!! DIE!");
        assert_eq!(f("DO YOU WANT 70 DIE?!"), "DO YOU WANT TO DIE?!");
        assert_eq!(f("I'M 50 TIRED"), "I'M SO TIRED");
        assert_eq!(f("YOU WANT 70."), "YOU WANT 70.", "no word after: a number");
        assert_eq!(f("VERY BAD EY ESIGHT"), "VERY BAD EYESIGHT");
        assert_eq!(f("4 N W-WHAT SHOULD WE DO FROM NOW ON? Y"), "W-WHAT SHOULD WE DO FROM NOW ON?");
        assert_eq!(f("GOING L 70 FIGHT?!"), "GOING TO FIGHT?!");
        assert_eq!(f("A HAVE"), "A HAVE", "A / I / O stay");
        assert_eq!(f("LV. 3"), "LV. 3", "too few words to judge");
        assert_eq!(f("YOU SHOULD STAY COOPED UP"), "YOU SHOULD STAY COOPED UP", "inner doubles stay");
        assert_eq!(f("WHAT DID YOU JUUST SAY"), "WHAT DID YOU JUST SAY", "unless it gives a common word");
        assert_eq!(f("5 GOLD? THAT STUPID"), "5 GOLD? THAT STUPID");
        assert_eq!(f("4 P HYUNG! WHAT SHOULD"), "HYUNG! WHAT SHOULD");
    }


    #[test]
    fn glued_common_words_beat_names() {
        let d = Dictionary::from_words(["Cando", "Andis", "can", "do", "and", "is", "it", "you", "instead"].map(String::from));
        let f = |t: &str| fix_tokens(t, &d, &mut 0);
        assert_eq!(f("YOU CANDO IT"), "YOU CAN DO IT");
        assert_eq!(f("ANDIS INSTEAD"), "AND IS INSTEAD");
    }


    #[test]
    fn long_letter_runs_are_fast() {
        let d = dict();
        let t = std::time::Instant::now();
        let mut n = 0;
        let _ = fix_tokens("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAH! WHAT", &d, &mut n);
        let _ = fix_tokens("WAAAAAAAAAHHHHHHHHHHHHHHHHHHHHHHHH!! HUH", &d, &mut n);
        assert!(t.elapsed().as_millis() < 100);
    }

}
