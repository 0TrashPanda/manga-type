//! Which text to type. The user's choice (2026-11-25), from examples in their chapters:
//!
//! - keep: speech bubbles (white, black, grey), narration and captions (over the art, in
//!   boxes, glowing);
//! - drop: sound effects ("KIEK!", "KYAAAAAK", Korean SFX misread as "LTH!"), game /
//!   status windows ("WARRIOR COMMON GRADE", "STRENGTH 11 RARE", "Class ..."), small
//!   handwritten side chatter on the art ("Shh... No way...", "Says the one who's just
//!   talking."), credits and logos (handled with the ads in `ocr::drop_ad_pages`).
//!
//! Runs on every load (rules change without a rescan). Uses the text, its case, and
//! `Bubble::framed` (how plain the background around each line is: ~1 in a bubble or box).
//! Bubbles the user edited are always kept.

use std::collections::HashSet;

use crate::fix::Dictionary;
use crate::ocr::Bubble;

/// `framed` at or above this: the lines sit on a plain bubble/box background.
const IN_BUBBLE: f32 = 0.85;

/// Stat names that start a status-window line ("STRENGTH 11 RARE", "ENDURANCE +1").
const STATS: &[&str] = &["STRENGTH", "DEXTERITY", "STAMINA", "ENDURAN", "LUCK", "INTELLIGENCE", "MAGIC POWER", "AGILITY", "VITALITY"];
const GRADES: &[&str] = &["COMMON", "RARE", "HEROIC", "LEGENDARY", "EPIC", "MYTHIC"];
/// Labels in short window lines ("HEROIC RANK", "WARRIOR COMMON GRADE", "TENDENCY").
const WINDOW_LABELS: &[&str] = &["GRADE", "RANK", "RATING", "TENDENCY", "SPECIALTY", "NAME AGE", "TALENT", "LV."];
/// System phrasing in window descriptions.
const SYSTEM_PHRASES: &[&str] = &[
    "INCREASES BY", "WILL INCREASE", "STAT INCREASE", "ACQUIRES ALL", "KNOWLEDGE ACQUIRED", "YOU WILL RECEIVE", "YOU WILL GAIN",
    "YOU'VE OPENED", "YOU HAVE OPENED",
];
/// Honorifics and kinship terms: part of names, not sound effects ("HYUNG-NIM", "JIHYE-SSI").
const HONORIFICS: &str = "NIM SSI SAN KUN CHAN SAMA SENPAI SUNBAE HYUNG OPPA NOONA UNNI EONNI AJUSSI AHJUSSI AJUMMA HALMEONI";
/// Window titles, set in mixed case ("Class") in otherwise all-caps lettering.
const WINDOW_TITLES: &[&str] = &["Class", "Status", "Quest", "System", "Skill", "Notice", "Reward", "Alert"];

/// Interjections are speech, not sound effects (after collapsing repeated letters).
const INTERJECTIONS: &str = "AH AHA EH HA HAH HAHA HE HEH HEHE HM HMM HMPH HUH OH OW UGH UH UM UR URK WOW YA YAH \
    TCH PHEW OOPS GASP WHOA HEY OI SIGH PF PFT EUGH ACK GAH NO YES OK OKAY HI BYE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Speech,
    SoundEffect,
    GameWindow,
    SideChatter,
}

fn letters_upper(w: &str) -> String {
    w.chars().filter(|c| c.is_alphabetic() || *c == '\'' || *c == '’').collect::<String>().to_uppercase()
}

/// "KYAAAAAK" -> "KYAK".
fn collapse(w: &str) -> String {
    let mut out = String::new();
    for c in w.chars() {
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

fn has_triple(w: &str) -> bool {
    let c: Vec<char> = w.chars().collect();
    c.windows(3).any(|t| t[0] == t[1] && t[1] == t[2])
}

/// Per-chapter context for [`classify`].
pub struct Context<'a> {
    dict: Option<&'a Dictionary>,
    /// Words from the chapter's longer lines: names ("KIYOUNG", "HYUNG-NIM") show up there.
    vocab: HashSet<String>,
    /// The chapter's lettering is all caps (so mixed-case text is something else).
    caps_lettering: bool,
    interjections: HashSet<&'static str>,
    honorifics: HashSet<&'static str>,
}

impl<'a> Context<'a> {
    pub fn new(bubbles: &[Bubble], dict: Option<&'a Dictionary>) -> Self {
        let mut vocab = HashSet::new();
        for b in bubbles {
            let words: Vec<&str> = b.text.split_whitespace().collect();
            if words.len() >= 4 {
                for w in words {
                    for part in w.split('-') {
                        let l = letters_upper(part);
                        if l.chars().count() >= 3 {
                            vocab.insert(l);
                        }
                    }
                }
            }
        }
        let caps = bubbles.iter().filter(|b| is_caps(&b.text)).count();
        Self {
            dict,
            vocab,
            caps_lettering: !bubbles.is_empty() && caps * 10 >= bubbles.len() * 7,
            interjections: INTERJECTIONS.split_whitespace().collect(),
            honorifics: HONORIFICS.split_whitespace().collect(),
        }
    }

    /// A real word, interjection or name (each part of "TH-THIS", "HYUNG-NIM" on its own).
    fn known(&self, word: &str) -> bool {
        word.split('-').all(|part| self.known_part(part))
    }

    fn known_part(&self, part: &str) -> bool {
        let w = letters_upper(part);
        if w.chars().count() < 3 {
            return true; // stammers ("TH-"), "I", "A", "OK"
        }
        let c = collapse(&w);
        if [&w, &c].iter().any(|x| self.interjections.contains(x.as_str()) || self.honorifics.contains(x.as_str())) {
            return true;
        }
        if self.dict.is_some_and(|d| d.contains(&w) || d.contains(&c)) {
            return true;
        }
        // A name from the chapter's sentences, maybe stretched or with a suffix
        // ("KIYOUNGSS", "HYUNGNIM").
        self.vocab.contains(&w)
            || self.vocab.iter().any(|v| v.chars().count() >= 4 && (w.starts_with(v.as_str()) || c.starts_with(v.as_str())))
            || self.honorifics.iter().any(|h| w.ends_with(h) && self.vocab.iter().any(|v| w.starts_with(v.as_str())))
    }
}

fn is_caps(text: &str) -> bool {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    let lower = letters.iter().filter(|c| c.is_lowercase()).count();
    lower * 10 <= letters.len()
}

pub fn classify(b: &Bubble, ctx: &Context) -> Kind {
    let text = b.text.trim();
    let upper = text.to_uppercase();
    let in_bubble = b.framed.is_none_or(|f| f >= IN_BUBBLE);
    let words: Vec<&str> = text.split_whitespace().collect();

    // Game windows, by the shape of the text: the background doesn't tell them apart (a
    // stat line can sit on a plain panel, speech in a busy bubble).
    let has_digit = text.chars().any(|c| c.is_ascii_digit());
    let titled = WINDOW_TITLES.iter().any(|t| text.starts_with(t) && text[t.len()..].chars().next().is_none_or(|c| !c.is_alphabetic()));
    let stat_line = STATS.iter().any(|s| upper.starts_with(s)) && (has_digit || words.len() <= 3);
    let graded = GRADES.iter().any(|g| upper.split(|c: char| !c.is_alphabetic()).any(|w| w == *g));
    // Whole words only: "FRUSTRATING" isn't a RATING.
    let upper_words: Vec<&str> = upper.split(|c: char| !c.is_alphanumeric() && c != '.').map(|w| w.trim_end_matches('.')).collect();
    let label = |l: &str| if l.contains(' ') || l.ends_with('.') { upper.contains(l) } else { upper_words.contains(&l) };
    let short_label = words.len() <= 4 && WINDOW_LABELS.iter().any(|l| label(l)) && !text.ends_with('?');
    let paren_grade = upper.contains("GRADE)") || upper.contains("RANK)");
    let system = SYSTEM_PHRASES.iter().any(|p| upper.contains(p));
    if titled || stat_line || (graded && has_digit && words.len() <= 6) || short_label || paren_grade || system {
        return Kind::GameWindow;
    }

    // Sound effects: one or two words, not ending like a sentence, with a stretched word
    // out on the art or a word that's neither English, an interjection nor a name.
    let trimmed = text.trim_end_matches(['!', '~']);
    let sentence_end = text.contains('?') && text.ends_with(['?', '!'])
        || trimmed.ends_with(['.', '?', '…', ','])
        || text.ends_with(',');
    if (1..=2).contains(&words.len()) && !has_digit && !sentence_end {
        let sfx = words.iter().any(|w| {
            let l = letters_upper(w);
            !l.is_empty() && ((has_triple(&l) && !in_bubble && !ctx.known(w)) || !ctx.known(w))
        });
        if sfx {
            return Kind::SoundEffect;
        }
    }

    // Side chatter: mixed-case handwriting on the art, in a chapter lettered in caps,
    // with no capital-letter speech left once the handwriting is taken out.
    if ctx.caps_lettering && !in_bubble && !is_caps(text) && caps_part(text).chars().filter(|c| c.is_alphabetic()).count() < 2 {
        return Kind::SideChatter;
    }
    Kind::Speech
}

/// The words without lowercase letters: the capital-letter speech of a bubble that got
/// merged with handwriting next to it ("HERE, HAVE SOME. Hehe" -> "HERE, HAVE SOME.").
fn caps_part(text: &str) -> String {
    text.split_whitespace().filter(|w| !w.chars().any(char::is_lowercase)).collect::<Vec<_>>().join(" ")
}

/// Keep what the user types: speech and narration. In a caps-lettered chapter, speech
/// outside a plain bubble loses the handwritten (mixed-case) words merged into it.
pub fn keep_typeable(bubbles: Vec<Bubble>, dict: Option<&Dictionary>) -> Vec<Bubble> {
    let ctx = Context::new(&bubbles, dict);
    bubbles
        .into_iter()
        .filter(|b| b.edited.is_some() || classify(b, &ctx) == Kind::Speech)
        .map(|mut b| {
            let in_bubble = b.framed.is_none_or(|f| f >= IN_BUBBLE);
            if b.edited.is_none() && ctx.caps_lettering && !in_bubble && !is_caps(&b.text) {
                let caps = caps_part(&b.text);
                if b.raw.is_none() {
                    b.raw = Some(b.text.clone());
                }
                b.text = caps;
            }
            b
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::Rect;

    fn b(t: &str, framed: f32) -> Bubble {
        Bubble { rect: Rect { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }, text: t.into(), conf: 99.0, edited: None, raw: None, framed: Some(framed) }
    }

    #[test]
    fn kinds_from_the_users_examples() {
        let dict = Dictionary::from_words(
            "I REALLY DIDN'T LIKE THAT GIRL FROM THE START EVENTUALLY THEY'LL STARVE TO DEATH LOOK CLOSELY \
             AURA GO NO WAY SAYS ONE WHO'S JUST TALKING WARRIOR COMMON GRADE STRENGTH RARE YOU WANT SHIT YES"
                .split_whitespace()
                .map(String::from),
        );
        let chapter = vec![
            b("I'LL GO.", 1.0),
            b("I REALLY DIDN'T LIKE THAT HAYAN GIRL FROM THE START.", 1.0),
            b("EVENTUALLY, THEY'LL STARVE TO DEATH.", 0.6),
            b("LOOK CLOSELY, LEE JIHYE.", 1.0),
            b("AURA!", 0.5),
            b("KIEK!", 0.96),
            b("KYAAAAAK", 0.87),
            b("LTH!", 0.4),
            b("WARRIOR COMMON GRADE", 0.7),
            b("STRENGTH 11 RARE", 0.4),
            b("Class YOU'VE OPENED A FEW CLASSES.", 0.3),
            b("Shh... No way...", 0.5),
            b("Says the one who's just talking.", 0.8),
            b("YES, FOR NOW, THERE'S WARRIOR, ARCHER, AND MAGICIAN FOR COMMON GRADE,", 0.95),
            b("HYUNG-NIM IS A COMMANDER! A COMMANDER!", 1.0),
            b("HYUNG-NIM!", 1.0),
            b("KIYOUNG OPPA, WHERE ARE YOU GOING?", 1.0),
            b("KIYOUNGSS!!!!", 0.86),
            b("DIKIA.", 1.0),
            b("HUH?", 1.0),
            b("AHHH...", 1.0),
            b("TH-THIS IS...?!", 0.91),
            b("SH-SHIT!", 0.99),
            b("YES, HYUNG-NIM!", 0.88),
            b("Hyung-nim,", 0.91),
            b("YOU HAVE 30 ENDURANCE STAT POINTS, AFTER ALL.", 0.81),
            b("HOW TO MANIPULATE MAGIC POWER, RIGHT?", 0.84),
            b("SHE POSSESSES LEGENDARY-RANK MAGIC POWER...!", 0.83),
            b("FOCUS ALL YOUR MIND AND STRENGTH AND...", 0.83),
            b("HERE, HAVE SOME. Hehe", 0.78),
            b("HEROIC RANK", 0.6),
            b("ENDURANCE +1", 0.83),
            b("MAGICIAN (COMMON-RANK)", 0.73),
            b("HEROIC-RANK. PERFECTLY ACQUIRES ALL KNOWLEDGE REGARDING THE FOUNDATIONS", 0.33),
            b("Status KIM HYUNSUNG 22 VIRTUOUS ARBITRATOR SWORDSMAN (COMMON RANK)", 0.7),
            b("NAME AGE", 0.84),
            b("Hehe", 0.62),
            b("AH... IT'S SO FRUSTRATING.", 0.94),
        ];
        let ctx = Context::new(&chapter, Some(&dict));
        let k = |t: &str| classify(chapter.iter().find(|x| x.text == t).unwrap(), &ctx);
        use Kind::*;
        for t in ["I'LL GO.", "I REALLY DIDN'T LIKE THAT HAYAN GIRL FROM THE START.", "EVENTUALLY, THEY'LL STARVE TO DEATH.",
                  "LOOK CLOSELY, LEE JIHYE.", "AURA!", "YES, FOR NOW, THERE'S WARRIOR, ARCHER, AND MAGICIAN FOR COMMON GRADE,",
                  "HYUNG-NIM!", "KIYOUNGSS!!!!", "DIKIA.", "HUH?", "AHHH...", "TH-THIS IS...?!", "SH-SHIT!", "YES, HYUNG-NIM!",
                  "Hyung-nim,", "YOU HAVE 30 ENDURANCE STAT POINTS, AFTER ALL.", "HOW TO MANIPULATE MAGIC POWER, RIGHT?",
                  "SHE POSSESSES LEGENDARY-RANK MAGIC POWER...!", "FOCUS ALL YOUR MIND AND STRENGTH AND...", "HERE, HAVE SOME. Hehe",
                  "AH... IT'S SO FRUSTRATING."] {
            assert_eq!(k(t), Speech, "{t}");
        }
        for t in ["KIEK!", "KYAAAAAK", "LTH!"] {
            assert_eq!(k(t), SoundEffect, "{t}");
        }
        for t in ["WARRIOR COMMON GRADE", "STRENGTH 11 RARE", "Class YOU'VE OPENED A FEW CLASSES.", "HEROIC RANK", "ENDURANCE +1",
                  "MAGICIAN (COMMON-RANK)", "HEROIC-RANK. PERFECTLY ACQUIRES ALL KNOWLEDGE REGARDING THE FOUNDATIONS",
                  "Status KIM HYUNSUNG 22 VIRTUOUS ARBITRATOR SWORDSMAN (COMMON RANK)", "NAME AGE"] {
            assert_eq!(k(t), GameWindow, "{t}");
        }
        for t in ["Shh... No way...", "Says the one who's just talking.", "Hehe"] {
            assert_eq!(k(t), SideChatter, "{t}");
        }
        let kept = keep_typeable(chapter.clone(), Some(&dict));
        assert!(kept.iter().any(|b| b.text == "HERE, HAVE SOME."), "handwriting stripped from merged speech");
    }
}
