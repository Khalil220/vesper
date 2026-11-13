use unicode_normalization::char::{decompose_canonical, decompose_compatible, is_combining_mark};

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

const MARK_OPENERS: &[(char, char)] = &[
    ('"', '"'),
    ('\'', '\''),
    ('\u{201c}', '\u{201d}'),
    ('\u{2018}', '\u{2019}'),
    ('\u{201d}', '\u{201d}'),
    ('\u{2019}', '\u{2019}'),
    ('(', ')'),
    ('[', ']'),
    ('<', '>'),
    ('*', '*'),
];

const LOOSE_CLOSERS: &[char] = &['.', '!', '?', ',', '*', '>'];

struct Site {
    needle: &'static str,
    name: &'static str,
    needs_tld: bool,
}

const SITES: &[Site] = &[
    Site { needle: "freewebnovel", name: "freewebnovel", needs_tld: false },
    Site { needle: "reewebnovel", name: "reewebnovel", needs_tld: false },
    Site { needle: "pandanov", name: "pandanovel", needs_tld: false },
    Site { needle: "andasnovel", name: "andasnovel", needs_tld: false },
    Site { needle: "lightnovelpub", name: "lightnovelpub", needs_tld: false },
    Site { needle: "lightsnovel", name: "lightsnovel", needs_tld: false },
    Site { needle: "novelfire", name: "novelfire", needs_tld: false },
    Site { needle: "novelphoenix", name: "novelphoenix", needs_tld: false },
    Site { needle: "eaglesnov", name: "eaglesnovel", needs_tld: false },
    Site { needle: "myboxnovel", name: "myboxnovel", needs_tld: false },
    Site { needle: "lightnovelworld", name: "lightnovelworld", needs_tld: true },
    Site { needle: "allnovelfull", name: "allnovelfull", needs_tld: true },
    Site { needle: "newnovel", name: "newnovel", needs_tld: true },
];

const LEAD_INS: &[&str] = &[
    "ifyouwanttoreadmorechapterspleasevisit",
    "ifyouwanttoreadmorechaptersvisit",
    "wanttoseemorechapterspleasevisit",
    "themostuptodatenovelsarepublishedon",
    "newnovelchaptersarepublishedon",
    "thelatestepisodesareonthe",
    "thischapterisupdateby",
    "readlatestchaptersat",
    "pleasereadingon",
    "thisupdatedby",
    "formorevisit",
    "readmoreon",
    "readmoreat",
    "updatefrom",
    "readingon",
    "google",
    "search",
    "visit",
];

const DANGLING_PITCHES: &[&str] = &[
    "ifyouwanttoreadmorechapterspleasevisit",
    "ifyouwanttoreadmorechaptersvisit",
    "wanttoseemorechapterspleasevisit",
    "readmorevisit",
    "formorevisit",
    "pleasevisit",
];

const TAILS: &[&str] = &[
    "toexperiencefasterupdatespeed",
    "forabetterexperience",
    "toreadpls",
    "please",
    "only",
];

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

const JUNK_BEFORE_TLD: usize = 2;

const BRANDS_ON_NOVEL: &[&str] = &["panda"];

const LEAK_OPENERS: &[&str] = &["more", "or discord", "or telegram"];

const LEAK_SIGNS: &[&str] = &["t.me/+", "buyers club"];

const COPY_SITE_NOTE: &str = "you are reading on our content copy site";

const COPY_SITE_END: &str = "to support us";

const COPY_SITE_GREETING: &str = "dear readers!";

const QUOTES: &[char] = &['"', '\'', '\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}'];

const SENTENCE_BREAKS: &[char] = &['.', '!', '?'];

const FRAGMENT_BREAKS: &[char] = &['.', '!', '?', '"', '\u{201c}', '\u{201d}'];

fn lookalike(c: char) -> Option<char> {
    Some(match c {
        'ᴀ' | 'α' | 'Α' | 'а' | 'А' => 'a',
        'ʙ' | 'Β' | 'в' | 'В' => 'b',
        'ᴄ' | 'с' | 'С' => 'c',
        'ᴅ' | 'ԁ' => 'd',
        'ᴇ' | 'ε' | 'Ε' | 'е' | 'Е' => 'e',
        'ꜰ' | 'ғ' => 'f',
        'ɢ' => 'g',
        'ʜ' | 'Η' | 'н' | 'Н' | 'һ' => 'h',
        'ɪ' | 'ı' | 'ι' | 'Ι' | 'і' | 'І' => 'i',
        'ᴊ' | 'ј' | 'Ј' => 'j',
        'ᴋ' | 'κ' | 'Κ' | 'к' | 'К' => 'k',
        'ʟ' => 'l',
        'ᴍ' | 'Μ' | 'м' | 'М' => 'm',
        'ɴ' | 'Ν' | 'п' => 'n',
        'ᴏ' | 'ο' | 'Ο' | 'о' | 'О' | '0' => 'o',
        'ᴘ' | 'ρ' | 'Ρ' | 'р' | 'Р' => 'p',
        'ꞯ' | 'ԛ' => 'q',
        'ʀ' => 'r',
        'ꜱ' | 'ѕ' | 'Ѕ' => 's',
        'ᴛ' | 'τ' | 'Τ' | 'т' | 'Т' => 't',
        'ᴜ' | 'υ' => 'u',
        'ᴠ' | 'ν' => 'v',
        'ᴡ' | 'ԝ' => 'w',
        'χ' | 'Χ' | 'х' | 'Х' => 'x',
        'ʏ' | 'Υ' | 'у' | 'У' => 'y',
        'ᴢ' | 'Ζ' => 'z',
        _ => return None,
    })
}

fn fold_char(c: char, mut push: impl FnMut(char)) {
    decompose_compatible(c, |d| {
        if is_combining_mark(d) {
            return;
        }
        match lookalike(d) {
            Some(latin) => push(latin),
            None => d.to_lowercase().for_each(&mut push),
        }
    });
}

fn is_styled(c: char) -> bool {
    if c.is_ascii() || !c.is_alphabetic() {
        return false;
    }
    let mut accented = false;
    decompose_canonical(c, |d| accented |= is_combining_mark(d));
    if accented {
        return false;
    }
    let mut latin = true;
    decompose_compatible(c, |d| latin &= d.is_ascii_alphabetic() || lookalike(d).is_some());
    latin
}

struct FoldedWord {
    text: String,
    start: usize,
    end: usize,
    capitalized: bool,
    styled: bool,
}

fn fold_words(text: &str) -> Vec<FoldedWord> {
    let mut words: Vec<FoldedWord> = Vec::new();
    let mut current: Option<FoldedWord> = None;

    for (i, c) in text.char_indices() {
        let end = i + c.len_utf8();
        let styled = is_styled(c);
        let mut folded = String::new();
        fold_char(c, |d| folded.push(d));
        for d in folded.chars() {
            if d.is_alphanumeric() {
                match current.as_mut() {
                    Some(word) => {
                        word.text.push(d);
                        word.end = end;
                        word.styled |= styled;
                    }
                    None => {
                        current = Some(FoldedWord {
                            text: d.to_string(),
                            start: i.max(words.last().map_or(0, |w| w.end)),
                            end,
                            capitalized: c.is_uppercase(),
                            styled,
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

pub fn fold(text: &str) -> String {
    fold_words(text).into_iter().map(|w| w.text).collect()
}

fn breaks_sentence(gap: &str) -> bool {
    gap.contains(['!', '?'])
        || gap
            .char_indices()
            .any(|(i, c)| c == '.' && gap[i + 1..].starts_with(char::is_whitespace))
}

fn is_preposition(word: &str) -> bool {
    PROMO_PREPOSITIONS.contains(&word)
}

fn absorb(text: &str, start: usize, end: usize) -> (usize, usize) {
    let openers: Vec<(usize, char)> = text[..start]
        .char_indices()
        .rev()
        .take_while(|(_, c)| MARK_OPENERS.iter().any(|(open, _)| open == c))
        .collect();
    let closes = |opener: Option<&(usize, char)>, c: char| {
        opener.is_some_and(|(_, o)| MARK_OPENERS.contains(&(*o, c)))
    };

    let mut paired = 0;
    let mut end = end;
    loop {
        let rest = &text[end..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let Some(c) = trimmed.chars().next() else { break };
        if closes(openers.get(paired), c) {
            paired += 1;
        } else if !LOOSE_CLOSERS.contains(&c) {
            break;
        }
        end += rest.len() - trimmed.len() + c.len_utf8();
    }

    let start = match paired {
        0 => start,
        n => openers[n - 1].0,
    };
    (start, end)
}

struct Folded<'a> {
    text: &'a str,
    words: Vec<FoldedWord>,
    flat: String,
    owner: Vec<usize>,
    starts: Vec<usize>,
}

impl<'a> Folded<'a> {
    fn new(text: &'a str) -> Self {
        let words = fold_words(text);
        let mut flat = String::new();
        let mut owner = Vec::new();
        let mut starts = Vec::with_capacity(words.len());
        for (index, word) in words.iter().enumerate() {
            starts.push(flat.len());
            flat.push_str(&word.text);
            owner.resize(flat.len(), index);
        }
        Self {
            text,
            words,
            flat,
            owner,
            starts,
        }
    }

    fn flat_end(&self, word: usize) -> usize {
        self.starts[word] + self.words[word].text.len()
    }

    fn gap(&self, before: usize) -> &str {
        &self.text[self.words[before].end..self.words[before + 1].start]
    }

    fn after(&self, word: usize) -> &str {
        let next = self.words.get(word + 1).map_or(self.text.len(), |w| w.start);
        &self.text[self.words[word].end..next]
    }

    fn word_starting_at(&self, at: usize) -> Option<usize> {
        let word = *self.owner.get(at)?;
        (self.starts[word] == at).then_some(word)
    }

    fn phrase_from(&self, at: usize, phrases: &[&str]) -> Option<usize> {
        self.word_starting_at(at)?;
        phrases.iter().find_map(|phrase| {
            let end = at + phrase.len();
            if !self.flat[at..].starts_with(phrase) {
                return None;
            }
            let last = self.owner[end - 1];
            (self.flat_end(last) == end).then_some(last)
        })
    }

    fn tld_from(&self, word: usize) -> Option<usize> {
        self.phrase_from(self.starts[word], TLDS)
    }

    fn tld_after(&self, word: usize) -> Option<usize> {
        (word + 1 < self.words.len())
            .then(|| self.tld_from(word + 1))
            .flatten()
    }

    fn tld_past_junk(&self, word: usize) -> Option<usize> {
        let mut at = word;
        for _ in 0..=JUNK_BEFORE_TLD {
            if let Some(tld) = self.tld_after(at) {
                return Some(tld);
            }
            let next = at + 1;
            let glued = next < self.words.len() && !self.gap(at).contains(char::is_whitespace);
            if !glued || self.words[next].text.chars().count() > 2 {
                return None;
            }
            at = next;
        }
        None
    }

    fn lead_in_before(&self, word: usize) -> Option<usize> {
        if word == 0 || breaks_sentence(self.gap(word - 1)) {
            return None;
        }
        let end = self.starts[word];
        LEAD_INS
            .iter()
            .filter(|phrase| self.flat[..end].ends_with(**phrase))
            .filter_map(|phrase| self.word_starting_at(end - phrase.len()))
            .min()
    }

    fn pitch_start_word(&self, preposition: usize) -> Option<usize> {
        let mut index = preposition;
        for _ in 0..PROMO_MAX_PITCH_WORDS {
            if index == 0 {
                return None;
            }
            let candidate = index - 1;
            if self.gap(candidate).contains(SENTENCE_BREAKS) {
                return None;
            }
            if self.words[candidate].capitalized {
                return Some(candidate);
            }
            index = candidate;
        }
        None
    }

    fn pitch_before(&self, word: usize) -> Option<usize> {
        let preposition = word.checked_sub(1)?;
        if !is_preposition(&self.words[preposition].text) {
            return None;
        }
        self.pitch_start_word(preposition)
            .filter(|&w| PITCH_OPENERS.contains(&self.words[w].text.as_str()))
    }

    fn tail_after(&self, word: usize) -> usize {
        if word + 1 >= self.words.len() || self.gap(word).contains(SENTENCE_BREAKS) {
            return word;
        }
        self.phrase_from(self.starts[word + 1], TAILS)
            .filter(|&last| {
                last + 1 == self.words.len() || self.after(last).contains(['.', '!', '?', '\n'])
            })
            .unwrap_or(word)
    }

    fn span(&self, first: usize, last: usize) -> (usize, usize) {
        absorb(
            self.text,
            self.words[first].start,
            self.words[self.tail_after(last)].end,
        )
    }

    fn names_a_site(&self, first: usize, last: usize, has_tld: bool, needs_tld: bool) -> bool {
        if needs_tld && !has_tld {
            return false;
        }
        let words = &self.words[first..=last];
        has_tld
            || words.iter().any(|w| w.styled)
            || words.iter().all(|w| w.text.chars().count() <= 2)
            || (first..last).all(|i| {
                let gap = self.gap(i);
                !gap.contains(char::is_whitespace) && !matches!(gap, "-" | "\u{2010}")
            })
    }

    fn site_spans(&self, spans: &mut Vec<(usize, usize)>) {
        for site in SITES {
            let mut from = 0;
            while let Some(hit) = self.flat[from..].find(site.needle).map(|i| i + from) {
                from = hit
                    + if self.flat[hit..].starts_with(site.name) {
                        site.name.len()
                    } else {
                        site.needle.len()
                    };
                let first = self.owner[hit];
                let last = self.owner[from - 1];
                let tld = self.tld_past_junk(last);
                if !self.names_a_site(first, last, tld.is_some(), site.needs_tld) {
                    continue;
                }
                let start = self
                    .lead_in_before(first)
                    .or_else(|| self.pitch_before(first))
                    .unwrap_or(first);
                spans.push(self.span(start, tld.unwrap_or(last)));
            }
        }
    }

    fn stripped_domain_spans(&self, spans: &mut Vec<(usize, usize)>) {
        for word in 1..self.words.len() {
            let Some(last) = self.tld_from(word) else { continue };
            let dotted = self.gap(word - 1).contains('.');
            if !dotted && !self.words[word].styled {
                continue;
            }
            if let Some(start) = self.lead_in_before(word) {
                spans.push(self.span(start, last));
            }
        }
    }

    fn pitch_tld_span(&self, spans: &mut Vec<(usize, usize)>) {
        let last = self.words.len() - 1;
        if last == 0 || !TLDS.contains(&self.words[last].text.as_str()) {
            return;
        }
        let mut index = last - 1;
        if self.words[index].text.contains("novel") && index > 0 {
            index -= 1;
        }
        if !is_preposition(&self.words[index].text) {
            return;
        }
        if let Some(start) = self
            .pitch_start_word(index)
            .filter(|&w| PITCH_OPENERS.contains(&self.words[w].text.as_str()))
        {
            spans.push(self.span(start, last));
        }
    }

    fn styled_tld_span(&self, spans: &mut Vec<(usize, usize)>) {
        let last = self.words.len() - 1;
        let word = &self.words[last];
        let written = &self.text[word.start..word.end];
        if !TLDS.contains(&word.text.as_str()) || written.chars().any(|c| c.is_ascii_alphabetic()) {
            return;
        }
        let before = if last == 0 { 0 } else { self.words[last - 1].end };
        if let Some(dot) = self.text[before..word.start].rfind('.') {
            spans.push((before + dot, self.text.len()));
        }
    }

    fn fragment(&self, first: usize, last: usize) -> Option<&str> {
        let words = &self.words[first..=last];
        let joined = &self.flat[self.starts[first]..self.flat_end(last)];
        let short = words.iter().all(|w| w.text.chars().count() <= 3);
        let styled = words.iter().any(|w| w.styled);
        let unbroken = (first..last).all(|i| !self.gap(i).contains(FRAGMENT_BREAKS));
        (short && styled && unbroken && joined.len() >= 4).then_some(joined)
    }

    fn edge_fragment_spans(&self, spans: &mut Vec<(usize, usize)>) {
        let count = self.words.len();
        let widest = count.min(4);
        if self.text[..self.words[0].start].trim().is_empty() {
            if let Some(k) = (1..=widest).rev().find(|&k| {
                self.fragment(0, k - 1)
                    .is_some_and(|f| SITES.iter().any(|site| site.name.ends_with(f)))
            }) {
                spans.push((self.words[0].start, self.words[k - 1].end));
            }
        }
        if let Some(k) = (1..=widest).rev().find(|&k| {
            self.fragment(count - k, count - 1)
                .is_some_and(|f| SITES.iter().any(|site| site.name.starts_with(f)))
        }) {
            spans.push((self.words[count - k].start, self.text.len()));
        }
    }

    fn brand_on_novel_spans(&self, spans: &mut Vec<(usize, usize)>) {
        for word in 0..self.words.len().saturating_sub(1) {
            let hyphenated = matches!(self.gap(word), "-" | "\u{2010}");
            if hyphenated
                && BRANDS_ON_NOVEL.contains(&self.words[word].text.as_str())
                && self.words[word + 1].text.starts_with("novel")
            {
                spans.push((self.words[word].start, self.words[word + 1].start));
            }
        }
    }

    fn dangling_pitch_span(&self, spans: &mut Vec<(usize, usize)>) {
        let last = self.words.len() - 1;
        if !self.after(last).trim().trim_end_matches(':').is_empty() {
            return;
        }
        let Some(first) = DANGLING_PITCHES
            .iter()
            .filter(|pitch| self.flat.ends_with(**pitch))
            .find_map(|pitch| self.word_starting_at(self.flat.len() - pitch.len()))
        else {
            return;
        };
        let before = self.text[..self.words[first].start].trim_end();
        if before.is_empty() || before.ends_with(['.', '!', '?', '"', '\u{201d}', '\u{2019}']) {
            spans.push((self.words[first].start, self.text.len()));
        }
    }

    fn empire_span(&self, spans: &mut Vec<(usize, usize)>) {
        let last = self.words.len() - 1;
        let word = &self.words[last];
        if last == 0 || word.text != "empire" || word.capitalized {
            return;
        }
        if !is_preposition(&self.words[last - 1].text) {
            return;
        }
        if let Some(start) = self.pitch_start_word(last - 1).filter(|&w| {
            PROMO_OPENERS
                .iter()
                .any(|opener| opener.eq_ignore_ascii_case(&self.words[w].text))
        }) {
            spans.push(self.span(start, last));
        }
    }
}

fn leak_spans(lower: &str, spans: &mut Vec<(usize, usize)>) {
    for opener in LEAK_OPENERS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(opener).map(|i| i + from) {
            from = at + opener.len();
            spans.extend(leak_at(lower, at, from));
        }
    }
}

fn leak_at(lower: &str, at: usize, opener_end: usize) -> Option<(usize, usize)> {
    if lower[opener_end..].starts_with(char::is_alphanumeric) {
        return None;
    }
    let bracketed = lower[..at].ends_with('(');
    let before = lower[..at].trim_end_matches([' ', '\t']);
    let opens_sentence = before.is_empty()
        || before.ends_with(['\n', '.', '!', '?', '"', '\u{201d}', '\u{2019}', ')']);
    if !bracketed && !opens_sentence {
        return None;
    }

    let line_end = lower[at..].find('\n').map_or(lower.len(), |i| at + i);
    let mut end = if bracketed {
        lower[at..line_end].find(')').map_or(line_end, |i| at + i + 1)
    } else {
        line_end
    };
    if !bracketed && lower[at..end].trim_end().ends_with(':') && end < lower.len() {
        let rest = &lower[end + 1..];
        let link = rest.trim_start_matches([' ', '\t']);
        if link.starts_with("t.me/+") || link.starts_with("https://t.me/+") {
            end += 1 + rest.find('\n').unwrap_or(rest.len());
        }
    }

    let start = if bracketed { at - 1 } else { at };
    LEAK_SIGNS
        .iter()
        .any(|sign| lower[at..end].contains(sign))
        .then_some((start, end))
}

fn copy_site_spans(text: &str, lower: &str, spans: &mut Vec<(usize, usize)>) {
    let mut from = 0;
    while let Some(at) = lower[from..].find(COPY_SITE_NOTE).map(|i| i + from) {
        let Some(end) = lower[at..]
            .find(COPY_SITE_END)
            .map(|i| at + i + COPY_SITE_END.len())
        else {
            break;
        };
        let start = lower[..at]
            .trim_end()
            .strip_suffix(COPY_SITE_GREETING)
            .map_or(at, str::len);
        spans.push(absorb(text, start, end));
        from = end;
    }
}

fn promo_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let lower = text.to_ascii_lowercase();
    leak_spans(&lower, &mut spans);
    copy_site_spans(text, &lower, &mut spans);

    let folded = Folded::new(text);
    if !folded.words.is_empty() {
        folded.site_spans(&mut spans);
        folded.brand_on_novel_spans(&mut spans);
        folded.stripped_domain_spans(&mut spans);
        folded.pitch_tld_span(&mut spans);
        folded.styled_tld_span(&mut spans);
        folded.edge_fragment_spans(&mut spans);
        folded.dangling_pitch_span(&mut spans);
        folded.empire_span(&mut spans);
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
            push_piece(&mut kept, &text[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    push_piece(&mut kept, &text[cursor..]);

    let tidy = kept
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let dialogue = tidy.contains(QUOTES) && tidy.chars().any(|c| !QUOTES.contains(&c) && !c.is_whitespace());
    (dialogue || tidy.chars().any(char::is_alphanumeric)).then_some(tidy)
}

fn push_piece(kept: &mut String, piece: &str) {
    let mut chars = piece.chars();
    let closes = match chars.next() {
        Some(c) if QUOTES.contains(&c) => chars.next().map_or(true, char::is_whitespace),
        Some(')' | ']' | '.' | ',' | '!' | '?') => true,
        _ => false,
    };
    if closes {
        kept.truncate(kept.trim_end().len());
    }
    kept.push_str(piece);
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
        for note in [
            "[To see the Visual Representation of Pip and Elle again, you can visit Discord.gg/NunuXD]",
            "A/N: yup. Clarissa is finally back! Her character arts are now up on discord and WebNovel, show my girl some love..",
            "NunuNote: You can find all Visual Representations at discord.gg/NunuXD",
            "More visuals of the guild hall are up at discord.gg/NunuXD",
            "Moreover, the group chat moved to t.me/guildhall last week.",
            "More art and early chapters are up at t.me/guildhall",
            "Hey guys, two more Chapters will be posted soon.",
            "Join the group chat for more chapters t.me/+AbCdEf",
            "PS: it’s a new week, new goals, if we’re able to hit a 100 power stones, there’ll be bonus Chapters at the end of the week..",
        ] {
            assert_eq!(strip_promo(note).as_deref(), Some(note), "{note:?}");
        }
    }

    fn assert_strips(cases: &[(&str, Option<&str>)]) {
        for (raw, want) in cases {
            assert_eq!(strip_promo(raw).as_deref(), *want, "input: {raw:?}");
        }
    }

    fn assert_keeps(prose: &[&str]) {
        for p in prose {
            assert_eq!(strip_promo(p).as_deref(), Some(*p), "{p:?} is prose");
        }
    }

    #[test]
    fn strips_a_domain_in_small_capitals() {
        assert_strips(&[
            (
                "\"Y-Yes…\" He nodded in a dazed manner. ꜰʀᴇᴇᴡᴇʙɴᴏᴠᴇʟ.ᴄᴏᴍ",
                Some("\"Y-Yes…\" He nodded in a dazed manner."),
            ),
            ("\"I know.\" ꜰʀᴇᴇwᴇʙɴovᴇʟ.coᴍ", Some("\"I know.\"")),
            (
                "She has heard stories and read about it before. Freeᴡebnᴏvel.cᴏm.",
                Some("She has heard stories and read about it before."),
            ),
            (
                "\"Are you okay, City Lord?!\" ᴜᴘᴅᴀᴛᴇ ꜰʀᴏᴍ ꜰʀᴇᴇᴡᴇʙɴᴏᴠᴇʟ.ᴄᴏᴍ.",
                Some("\"Are you okay, City Lord?!\""),
            ),
            (
                "Tian Yanyu asked, \"Can you help us?\" Read latest chapters at Freeᴡebnovel.cᴏm Only.",
                Some("Tian Yanyu asked, \"Can you help us?\""),
            ),
            (
                "Yuan was not only able to survive the giant's attack but even stop it. ꜰʀᴇᴇ ᴡᴇʙ ɴᴏᴠᴇʟ",
                Some("Yuan was not only able to survive the giant's attack but even stop it."),
            ),
        ]);
    }

    #[test]
    fn strips_every_observed_panda_novel_mark() {
        assert_strips(&[
            (
                "Yuan smiled at their frowning faces.If you want to read more chapters, visit pa nda-novel,c.o,m",
                Some("Yuan smiled at their frowning faces."),
            ),
            (
                "If you want to read more chapters, visit pa nda-novel,c.o,m. When Yuan heard the music, he felt sick.",
                Some("When Yuan heard the music, he felt sick."),
            ),
            (
                "\"We will see each other again one day. p an da no v el I promise that I will find you.\"",
                Some("\"We will see each other again one day. I promise that I will find you.\""),
            ),
            (
                "\"This is your first time seeing a demon. google p an da no v el It's going to be horrifying.\"",
                Some("\"This is your first time seeing a demon. It's going to be horrifying.\""),
            ),
            (
                "\"I can fly for an hour.\" Want to see more chapters? Please visit p a n d a -n o v e l .c o m",
                Some("\"I can fly for an hour.\""),
            ),
            (
                "ᴘ ᴀɴᴅ ᴀ ɴ ᴏᴠᴇʟ \"Yuan wouldn't lie to us!\" Huang Xiao Li backed him up.",
                Some("\"Yuan wouldn't lie to us!\" Huang Xiao Li backed him up."),
            ),
            (
                "pαпdα-ňᴏνê|·сóМ However, their purpose was not to clash head-on.",
                Some("However, their purpose was not to clash head-on."),
            ),
            (
                "pαndα`noν?1--сoМ It took him 5 hours to finish reading the complete document.",
                Some("It took him 5 hours to finish reading the complete document."),
            ),
            (
                "ραndαsnοvεl.cοm This same pattern of devastation repeated itself.",
                Some("This same pattern of devastation repeated itself."),
            ),
            (
                "andasnovel.com Despite their expressions, nobody moved.",
                Some("Despite their expressions, nobody moved."),
            ),
        ]);
    }

    #[test]
    fn strips_the_site_name_glued_onto_novel() {
        assert_strips(&[
            (
                "Oddly enough, a few were even close to the truth. Some panda-novel addicts believed it was the dawning of a mass mutation.",
                Some("Oddly enough, a few were even close to the truth. Some novel addicts believed it was the dawning of a mass mutation."),
            ),
            (
                "“This isn’t a cultivation novel,” Aron laughed. He wondered when she had found the time to read those strange panda-novels.",
                Some("“This isn’t a cultivation novel,” Aron laughed. He wondered when she had found the time to read those strange novels."),
            ),
            (
                "One intrepid conspiracy theorist, who had been spoiled by panda-novels, even privately believed it.",
                Some("One intrepid conspiracy theorist, who had been spoiled by novels, even privately believed it."),
            ),
        ]);
        assert_keeps(&[
            "She wrote a panda novel about bamboo forests.",
            "Pandamonium broke out in the hall.",
            "He bought a panda-shaped cookie for her.",
            "The red panda. Novel ideas came to him at night.",
        ]);
    }

    #[test]
    fn strips_a_name_split_across_two_paragraphs() {
        assert_strips(&[
            (
                "\"Save your energy.\" Feng Yuxiang said to him.ᴘᴀ ɴ ᴅᴀ",
                Some("\"Save your energy.\" Feng Yuxiang said to him."),
            ),
            (
                "n ᴏ ᴠᴇʟ\"No, I will challenge the Pagoda by myself.\" Yuan said to them.",
                Some("\"No, I will challenge the Pagoda by myself.\" Yuan said to them."),
            ),
        ]);
    }

    #[test]
    fn strips_a_domain_broken_up_with_slashes() {
        assert_strips(&[
            ("For more, visit lig/htno/v/elpub[.]com", None),
            ("For more, visit ligh/tnov/elpub[.]//com", None),
            ("For more, visit //lightnovel/pub[.]com", None),
            ("The most up-to-date novels are published on li/ghtn/ovelpub[.]/c/om", None),
            ("Visit lightn///ove/lpub[.]com for a better experience", None),
            ("The most up-to-date novels are published on li/ghtnovelworld[.]c/om", None),
            ("Visit /lightnovelworld[.]com for a better experience", None),
        ]);
    }

    #[test]
    fn strips_a_pitch_whose_name_was_already_removed() {
        assert_strips(&[
            ("For more, visit [.]com", None),
            ("For more, visit [./]c//om", None),
            ("Visit //[.]c/om for a better experience", None),
            ("The most up-to-date novels are published on /[.]c/om", None),
            ("\"By pure luck!\" ɴᴇᴡ ɴᴏᴠᴇʟ ᴄʜᴀᴘᴛᴇʀs ᴀʀᴇ ᴘᴜʙʟɪsʜᴇᴅ ᴏɴ ᴄᴏᴍ", Some("\"By pure luck!\"")),
            ("\"...\" ᴛʜɪs ᴄʜᴀᴘᴛᴇʀ ɪs ᴜᴘᴅᴀᴛᴇ ʙʏ ᴄᴏᴍ.", Some("\"...\"")),
            ("A female human at that. .ᴄᴏᴍ", Some("A female human at that.")),
            (". ᴄᴏᴍ", None),
        ]);
    }

    #[test]
    fn strips_marks_from_other_aggregators() {
        assert_strips(&[
            (
                "\"It’s a slap to the face of our seniors overseeing the test-\"The latest_epɪ_sodes are on_the NovᴇlFirenᴇt",
                Some("\"It’s a slap to the face of our seniors overseeing the test-\""),
            ),
            (
                "He could prove his identity wherever he went in the Mortal Realm.ʀᴇᴀᴅ ʟᴀᴛᴇsᴛ ᴄʜᴀᴘᴛᴇʀs ᴀᴛ novel•ﬁre.net",
                Some("He could prove his identity wherever he went in the Mortal Realm."),
            ),
            (
                "He had no reason to risk offending the Duvelle family.Thɪs updatᴇd by novel~fire~net",
                Some("He had no reason to risk offending the Duvelle family."),
            ),
            (
                "eαglesnᴏνel At the moment, everyone had a serious expression on their face.",
                Some("At the moment, everyone had a serious expression on their face."),
            ),
            (
                "eaglesnovɐ1,сoМ Lances clashed and the two seemed evenly matched.",
                Some("Lances clashed and the two seemed evenly matched."),
            ),
            (
                "Yuan proceeded to tell her about his plans. All ɴᴏᴠᴇʟ full.com",
                Some("Yuan proceeded to tell her about his plans."),
            ),
            (
                "ɪғ ʏᴏᴜ ᴡᴀɴᴛ ᴛᴏ ʀᴇᴀᴅ ᴍᴏʀᴇ ᴄʜᴀᴘᴛᴇʀs, ᴘʟᴇᴀsᴇ ᴠɪsɪᴛ ᴀʟʟɴᴏᴠᴇʟFᴜʟʟ.ᴄᴏᴍ ᴛᴏ ᴇxᴘᴇʀɪᴇɴᴄᴇ ғᴀsᴛᴇʀ ᴜᴘᴅᴀᴛᴇ sᴘᴇᴇᴅ.",
                None,
            ),
            ("Please reading- on MYB0 X N O VEL. COM", None),
            ("Reading on Mybo xno vel. com ,Please!", None),
            ("Visit (Myb o xn ov e l. com) to read, pls!", None),
            (
                "He had the first ten years as a foundation, “Read more on newn0vel. org” in just two days.",
                Some("He had the first ten years as a foundation, in just two days."),
            ),
            (
                "He understood why the Grace House was special as a royal farm.”Read more on newn0vel<org”",
                Some("He understood why the Grace House was special as a royal farm."),
            ),
            (
                "She became the only ally they had. “Read more on new n0vel.o rg”",
                Some("She became the only ally they had."),
            ),
            ("Read more at Novel~Phoenix", None),
            (
                "lightsnοvεl.cοm General Nzagi frowned and looked at the map.",
                Some("General Nzagi frowned and looked at the map."),
            ),
        ]);
    }

    #[test]
    fn strips_the_leak_groups_promotions() {
        assert_strips(&[
            (
                "'Ethea.'\nMore chapters in Discord server \"Buyers Club for Webnovels\" https://discord.gg/VuYnZcXyrC\nOR Telegram: https://t.me/+GqZCSNPbwVs3MTcy",
                Some("'Ethea.'"),
            ),
            ("\"Haha!\"\nMORE chapters in Telegram: t.me/+Z9CFfQjA6p9iM2My", Some("\"Haha!\"")),
            ("More chapters: t.me/+Z9CFfQjA6p9iM2My", None),
            ("More: t.me/+Z9CFfQjA6p9iM2My", None),
            ("More in t.me/+Z9CFfQjA6p9iM2My", None),
            ("More chapters in Telegram: \nt.me/+Z9CFfQjA6p9iM2My", None),
            (
                "Yuan could casually give away such a precious item. More chapters in Telegram t.me/+Z9CFfQjA6p9iM2My",
                Some("Yuan could casually give away such a precious item."),
            ),
            (
                "He had never seen it before. More chapters in Telegram with a link: t.me/+Z9CFfQjA6p9iM2My\nThe bidding went on.",
                Some("He had never seen it before.\nThe bidding went on."),
            ),
            (
                "\"That was a quick response,\" she said.(More chapters in Buyers club for TMT)",
                Some("\"That was a quick response,\" she said."),
            ),
        ]);
    }

    #[test]
    fn strips_a_pitch_left_without_its_domain() {
        assert_strips(&[
            (
                "Li Jinxi is a threat if she refuses to be on their side.If you want to read more chapters, visit",
                Some("Li Jinxi is a threat if she refuses to be on their side."),
            ),
            ("For more, visit:", None),
            ("Read more, visit:", None),
            (
                "The blow sent out a small shockwave.please visit",
                Some("The blow sent out a small shockwave."),
            ),
        ]);
        assert_keeps(&[
            "He told me to visit.",
            "If you want to read more, visit the library.",
            "\"Please visit.\"",
            "He asked her to please visit",
            "For more, visit the archive.",
        ]);
    }

    #[test]
    fn strips_the_copy_site_note() {
        assert_strips(&[
            (
                "Dear readers! you are reading on our content copy site.Please copy and search this link \" https://bom.so/sBJMIa \" to support us",
                None,
            ),
            (
                "This was true for the ghosts.’you are reading on our content copy site.Please copy and search this link ” https://bit.ly/3NSnQtR ” to support us’? They often suffered losses.",
                Some("This was true for the ghosts. They often suffered losses."),
            ),
        ]);
    }

    #[test]
    fn keeps_prose_that_brushes_against_a_site_name() {
        assert_keeps(&[
            "\"It's like reading a novel. Compared to the ending, the process is what makes people curious.\"",
            "It was the current bestselling light novel published by their company, Old Gods.",
            "He didn’t blush and turn away like a flustered generic light-novel protagonist.",
            "The latest episodes are on the net.",
            "Come visit. Freedom awaits you in the north.",
            "He kept a red panda. Novel ideas came to him at night.",
            "The phoenix novel sold well, and the new novel was a hit.",
            "Sunny сhoked. But his Flaw was merciless.",
            "[ℜ𝔈𝔖𝔗𝔒ℜ𝔈!]",
            "\"ᛞᛁᛖ\"",
            "Lu Ce: ?(´･ω･`)?",
            "[You received 3% shares of TṢMC] $37.860b",
        ]);
    }

    #[test]
    fn survives_a_character_that_folds_into_two_words() {
        assert_keeps(&["It weighed ½ a pound, then ¼ more."]);
        assert_eq!(
            strip_promo("It weighed ½ a pound. freewebnovel.com").as_deref(),
            Some("It weighed ½ a pound.")
        );
    }

    #[test]
    fn keeps_the_closing_quote_of_the_dialogue_a_mark_follows() {
        assert_eq!(
            strip_promo("\"Hold on freewebnovel.com\"").as_deref(),
            Some("\"Hold on\"")
        );
    }
}
