use unicode_normalization::char::{decompose_compatible, is_combining_mark};

const PROMO_PREPOSITIONS: &[&str] = &["on", "at", "from", "with", "by", "through", "via", "to"];

const PROMO_MAX_PITCH_WORDS: usize = 6;

const PROMO_OPENERS: &[&str] = &[
    "Continue",
    "Discover",
    "Enjoy",
    "Experience",
    "Explore",
    "Find",
    "Read",
    "Stay",
    "Your",
];

const MARK_OPENERS: &[char] = &['"', '\'', '\u{201c}', '\u{2018}', '(', '[', '<', '*'];

const MARK_CLOSERS: &[char] = &[
    '"', '\'', '\u{201d}', '\u{2019}', ')', ']', '>', '*', '.', '!', ',',
];

const SITE_NAMES: &[&str] = &["freewebnovel", "reewebnovel"];

const PITCH_OPENERS: &[&str] = &[
    "continue",
    "discover",
    "enjoy",
    "experience",
    "explore",
    "find",
    "read",
    "stay",
    "your",
    "updates",
    "please",
];

const TLDS: &[&str] = &["com", "net", "org"];

struct FoldedWord {
    text: String,
    start: usize,
    end: usize,
    capitalized: bool,
}

fn fold_words(text: &str) -> Vec<FoldedWord> {
    let mut words: Vec<FoldedWord> = Vec::new();
    let mut current: Option<FoldedWord> = None;

    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        let mut folded = String::new();
        decompose_compatible(c, |d| {
            if !is_combining_mark(d) {
                folded.push(d);
            }
        });
        for d in folded.chars().flat_map(char::to_lowercase) {
            if d.is_alphanumeric() {
                match current.as_mut() {
                    Some(word) => {
                        word.text.push(d);
                        word.end = end;
                    }
                    None => {
                        current = Some(FoldedWord {
                            text: d.to_string(),
                            start: i,
                            end,
                            capitalized: c.is_uppercase(),
                        })
                    }
                }
            } else if let Some(word) = current.take() {
                words.push(word);
            }
        }
    }
    words.extend(current);
    words
}

fn flatten(words: &[FoldedWord]) -> (String, Vec<usize>) {
    let mut flat = String::new();
    let mut owner = Vec::new();
    for (index, word) in words.iter().enumerate() {
        for c in word.text.chars() {
            for _ in 0..c.len_utf8() {
                owner.push(index);
            }
        }
        flat.push_str(&word.text);
    }
    (flat, owner)
}

fn absorb_openers(text: &str, start: usize) -> usize {
    let mut at = start;
    while let Some(c) = text[..at].chars().next_back() {
        if MARK_OPENERS.contains(&c) {
            at -= c.len_utf8();
        } else {
            break;
        }
    }
    at
}

fn absorb_closers(text: &str, end: usize) -> usize {
    let mut at = end;
    loop {
        let rest = &text[at..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let gap = rest.len() - trimmed.len();
        match trimmed.chars().next() {
            Some(c) if MARK_CLOSERS.contains(&c) => at += gap + c.len_utf8(),
            _ => return at,
        }
    }
}

fn is_preposition(word: &str) -> bool {
    PROMO_PREPOSITIONS.contains(&word)
}

fn preposition_before(words: &[FoldedWord], index: usize) -> Option<usize> {
    (index > 0 && is_preposition(&words[index - 1].text)).then(|| index - 1)
}

fn pitch_start_word(text: &str, words: &[FoldedWord], preposition: usize) -> Option<usize> {
    let mut index = preposition;
    for _ in 0..PROMO_MAX_PITCH_WORDS {
        if index == 0 {
            return None;
        }
        let candidate = index - 1;
        if text[words[candidate].end..words[index].start].contains(['.', '!', '?']) {
            return None;
        }
        if words[candidate].capitalized {
            return Some(candidate);
        }
        index = candidate;
    }
    None
}

fn tld_at(flat: &str, at: usize) -> Option<usize> {
    TLDS.iter()
        .find(|tld| flat[at..].starts_with(**tld))
        .map(|tld| at + tld.len())
}

fn promo_spans(text: &str) -> Vec<(usize, usize)> {
    let words = fold_words(text);
    if words.is_empty() {
        return Vec::new();
    }
    let (flat, owner) = flatten(&words);
    let mut spans: Vec<(usize, usize)> = Vec::new();

    for name in SITE_NAMES {
        let mut from = 0;
        while let Some(hit) = flat[from..].find(name).map(|i| i + from) {
            let name_end = hit + name.len();
            from = name_end;
            let first = owner[hit];
            let last = owner[name_end - 1];
            let letter_by_letter = words[first..=last]
                .iter()
                .all(|w| w.text.chars().count() <= 2);
            if last > first && !letter_by_letter {
                continue;
            }
            let end_word = match tld_at(&flat, name_end) {
                Some(tld_end) => owner[tld_end - 1],
                None => last,
            };
            let start_word = preposition_before(&words, first)
                .and_then(|p| pitch_start_word(text, &words, p))
                .filter(|&w| PITCH_OPENERS.contains(&words[w].text.as_str()))
                .unwrap_or(first);
            spans.push((
                absorb_openers(text, words[start_word].start),
                absorb_closers(text, words[end_word].end),
            ));
        }
    }

    let last = words.len() - 1;
    if last > 0 && TLDS.contains(&words[last].text.as_str()) {
        let mut index = last - 1;
        if words[index].text.contains("novel") && index > 0 {
            index -= 1;
        }
        if is_preposition(&words[index].text) {
            if let Some(start) = pitch_start_word(text, &words, index) {
                if PITCH_OPENERS.contains(&words[start].text.as_str()) {
                    spans.push((
                        absorb_openers(text, words[start].start),
                        absorb_closers(text, words[last].end),
                    ));
                }
            }
        }
    }

    if last > 0 && words[last].text == "empire" && !words[last].capitalized {
        if is_preposition(&words[last - 1].text) {
            if let Some(start) = pitch_start_word(text, &words, last - 1) {
                if PROMO_OPENERS
                    .iter()
                    .any(|opener| opener.eq_ignore_ascii_case(&words[start].text))
                {
                    spans.push((
                        absorb_openers(text, words[start].start),
                        absorb_closers(text, words[last].end),
                    ));
                }
            }
        }
    }

    spans.sort_unstable();
    spans
}

pub fn strip_promo(paragraph: &str) -> Option<String> {
    let text = paragraph.trim();
    let spans = promo_spans(text);
    if spans.is_empty() {
        return (!text.is_empty()).then(|| text.to_string());
    }

    let mut kept = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in spans {
        if start > cursor {
            kept.push_str(&text[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    kept.push_str(&text[cursor..]);

    let tidy = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    tidy.chars().any(char::is_alphanumeric).then_some(tidy)
}

pub fn strip_promo_paragraphs(paragraphs: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = paragraphs.iter().filter_map(|p| strip_promo(p)).collect();
    if cleaned.is_empty() {
        return paragraphs.to_vec();
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKS: &[&str] = &[
        "Enjoy exclusive adventures from freewebnovel",
        "Updates by Freewebnovel. com",
        "Experience exclusive tales on freewebnovel.com",
        "Continue -reading on Freewebnovel.com",
        "Your next journey awaits at freewebnovel",
        "--- Discover stories with empire",
        "Continue your saga on empire",
        "Stay tuned for updates on empire",
        "Read the latest on freewebnovel",
    ];

    const PROSE: &[&str] = &[
        "Eldorath Empire.",
        "Iron Empire.",
        "\"The Empire is protected by the mighty War God. Of course they're not afraid to burn a temple.\"",
        "Followers of War burned the temples of Shadow, and their empire spread, consuming many weaker realms.\"",
        "'So the Nine were determined to destroy the Empire...'",
        "\"WELCOME, LADIES AND GENTLEMEN, TO THE EMPIRE'S GRAND YOUTH TOURNAMENT!\"",
        "He was a soldier of a militant empire which worshiped War God and had conquered many lands.",
    ];

    #[test]
    fn drops_a_paragraph_that_is_only_a_mark() {
        for m in MARKS {
            assert_eq!(strip_promo(m), None, "{m:?} should be dropped");
        }
    }

    #[test]
    fn keeps_prose_about_an_in_story_empire() {
        for p in PROSE {
            assert_eq!(strip_promo(p).as_deref(), Some(*p), "{p:?} is prose");
        }
    }

    #[test]
    fn strips_a_mark_appended_to_a_real_paragraph() {
        assert_eq!(
            strip_promo("\"What do you want, Derek?\" Alice asked coldly. Find more chapters on empire")
                .as_deref(),
            Some("\"What do you want, Derek?\" Alice asked coldly.")
        );
        assert_eq!(
            strip_promo("Bang! Your adventure continues at empire").as_deref(),
            Some("Bang!")
        );
        assert_eq!(
            strip_promo(
                "Max sighed. \"Thorne Family sure is rich,\" he muttered, smiling. \
                 Find your next read on freewebnovel.com"
            )
            .as_deref(),
            Some("Max sighed. \"Thorne Family sure is rich,\" he muttered, smiling.")
        );
    }

    #[test]
    fn filters_a_whole_chapter() {
        let paragraphs: Vec<String> = [
            "He drew his sword.",
            "Enjoy more content from freewebnovel",
            "The blade sang. Stay updated with freewebnovel",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            strip_promo_paragraphs(&paragraphs),
            vec!["He drew his sword.".to_string(), "The blade sang.".to_string()]
        );
    }

    #[test]
    fn strips_marks_the_punctuation_hid() {
        let cases = [
            (
                "Then\u{2014}without hesitation\u{2014} Enjoy exclusive content from freewebnovel",
                "Then\u{2014}without hesitation\u{2014}",
            ),
            (
                "[Energy decreased by 0.1] Discover more stories at freewebnovel",
                "[Energy decreased by 0.1]",
            ),
            (
                "Just when the countdown reach 10, 9, 8\u{2026} Find your next read on empire",
                "Just when the countdown reach 10, 9, 8\u{2026}",
            ),
            ("[Max] Explore stories at empire", "[Max]"),
            (
                "\u{2013} Battle Coins: [9240] Stay updated via empire",
                "\u{2013} Battle Coins: [9240]",
            ),
            (
                "See you in the Battle Realm at noon tomorrow.] Read exclusive adventures at empire",
                "See you in the Battle Realm at noon tomorrow.]",
            ),
            (
                "ordinary people would not last so long.\u{201c}Please reading on Freewebnovel.com >\u{201d}",
                "ordinary people would not last so long.",
            ),
            (
                "Those were the Mad Blood weapons. \u{2018}Freewebnovel.com*\u{2019}It turned out that they were gone.",
                "Those were the Mad Blood weapons. It turned out that they were gone.",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(strip_promo(raw).as_deref(), Some(want), "input: {raw:?}");
        }
    }

    #[test]
    fn keeps_an_empire_that_the_sentence_carries_past() {
        for prose in [
            "In the war with empire forces, the column marched on.",
            "He rode to empire lands and never came back.",
        ] {
            assert_eq!(strip_promo(prose).as_deref(), Some(prose), "{prose:?}");
        }
    }

    #[test]
    fn keeps_prose_shaped_like_a_mark() {
        for prose in [
            "The road to empire.",
            "Their long march to empire.",
            "A thousand years of war, and every step on the road to empire.",
        ] {
            assert_eq!(strip_promo(prose).as_deref(), Some(prose), "{prose:?}");
        }
    }

    #[test]
    fn catches_every_observed_empire_mark() {
        for mark in [
            "Continue reading on empire",
            "Continue your saga on empire",
            "Discover hidden content at empire",
            "Discover more stories at empire",
            "Discover stories with empire",
            "Enjoy exclusive content from empire",
            "Enjoy more content from empire",
            "Enjoy new chapters from empire",
            "Experience more on empire",
            "Experience tales at empire",
            "Explore more at empire",
            "Explore stories at empire",
            "Find adventures on empire",
            "Find more chapters on empire",
            "Find your next adventure on empire",
            "Find your next read at empire",
            "Find your next read on empire",
            "Read exclusive adventures at empire",
            "Read new adventures at empire",
            "Read the latest on empire",
            "Stay tuned for updates on empire",
            "Stay updated through empire",
            "Stay updated via empire",
            "Your adventure continues at empire",
        ] {
            assert_eq!(strip_promo(mark), None, "{mark:?} should be dropped");
            let appended = format!("She turned away. {mark}");
            assert_eq!(
                strip_promo(&appended).as_deref(),
                Some("She turned away."),
                "{appended:?}"
            );
        }
    }

    #[test]
    fn a_chapter_of_nothing_but_marks_is_left_alone() {
        let only = vec!["Read the latest on freewebnovel".to_string()];
        assert_eq!(strip_promo_paragraphs(&only), only);
    }

    #[test]
    fn drops_a_domain_written_in_styled_unicode() {
        for styled in [
            "𝑓𝑟𝑒𝘦𝓌𝑒𝑏𝑛𝑜𝘷𝑒𝘭.𝒸𝘰𝑚",
            "𝐟𝐫𝕖𝗲𝘄𝚎𝗯𝕟𝐨𝕧𝐞𝚕.𝕔𝕠𝐦",
            "𝓯𝓻𝓮𝙚𝙬𝓮𝙗𝒏𝙤𝒗𝙚𝙡.𝒄𝒐𝓶",
            "𝙧𝙚𝙚𝔀𝒆𝓫𝓷𝙤𝓿𝒆𝙡.𝒄𝙤𝓶",
        ] {
            assert_eq!(strip_promo(styled), None, "{styled:?}");
        }
    }

    #[test]
    fn strips_a_domain_spelled_out_letter_by_letter() {
        assert_eq!(
            strip_promo("\"Who did this to the demons?\" f r e e w e b n o v e l. c o m").as_deref(),
            Some("\"Who did this to the demons?\"")
        );
    }

    #[test]
    fn strips_a_pitch_whose_domain_is_only_a_tld() {
        assert_eq!(
            strip_promo("The loot was gone. Explore more stories with .Côm").as_deref(),
            Some("The loot was gone.")
        );
        assert_eq!(strip_promo("Your next read is at .Côm"), None);
    }

    #[test]
    fn strips_a_pitch_for_another_novel_site() {
        assert_eq!(strip_promo("Updates by vipnovel.com"), None);
    }

    #[test]
    fn keeps_prose_that_ends_on_a_preposition_before_a_domain() {
        assert_eq!(
            strip_promo("\"What's going on\" 𝑓𝑟𝑒𝘦𝓌𝑒𝑏𝑛𝑜𝘷𝑒𝘭.𝒸𝘰𝑚").as_deref(),
            Some("\"What's going on\"")
        );
        assert_eq!(
            strip_promo("Then someone stepped through 𝐟𝐫𝕖𝗲𝘄𝚎𝗯𝕟𝐨𝕧𝐞𝚕.𝕔𝕠𝐦").as_deref(),
            Some("Then someone stepped through")
        );
    }

    #[test]
    fn keeps_prose_that_says_free_web_novel() {
        let prose = "He downloaded a free web novel last night.";
        assert_eq!(strip_promo(prose).as_deref(), Some(prose));
    }

    #[test]
    fn keeps_an_authors_note_linking_elsewhere() {
        let note = "Join our Discord server \"Buyers Club for Webnovels\" https://discord.gg/VuYnZcXyrC";
        assert_eq!(strip_promo(note).as_deref(), Some(note));
    }
}
