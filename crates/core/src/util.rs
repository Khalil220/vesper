//! Small cross-cutting helpers.

/// Normalize a novel title for cross-source identity: lowercase, keep only
/// alphanumerics (dropping spaces and punctuation). Lets the *same* novel titled
/// slightly differently across sites ("Shadow Slave", "shadow-slave") compare
/// equal, so a re-`subscribe` can be recognized as a duplicate.
pub fn normalize_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Make a string safe to use as a Windows filename.
///
/// Strips the characters Windows forbids (`<>:"/\|?*` and control chars),
/// trailing dots/spaces, and avoids reserved device names (CON, PRN, ...).
/// Never returns an empty string.
pub fn sanitize_filename(name: &str) -> String {
    const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

    let mut s: String = name
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();

    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    let trimmed = s.trim().to_string();

    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let base_upper = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let out = if RESERVED.contains(&base_upper.as_str()) {
        format!("_{trimmed}")
    } else {
        trimmed
    };

    // A title made entirely of illegal characters sanitizes to underscores,
    // which is a useless filename; fall back to a placeholder.
    if out.is_empty() || out.chars().all(|c| c == '_') {
        "untitled".to_string()
    } else {
        out
    }
}

/// Current wall-clock time as Unix seconds. Used for DB timestamps.
pub fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Strip a leading "Chapter N -/–/:/|" prefix from a table-of-contents link's
/// text, leaving just the chapter's actual name. ToC entries look like
/// "Chapter 1 - Cultivation Online"; we render our own "Chapter N:" prefix, so
/// keeping the site's would double it. If the text is *only* the prefix (no real
/// name), the original is returned unchanged.
pub fn clean_chapter_title(raw: &str) -> String {
    let t = raw.trim();
    let bytes = t.as_bytes();

    if t.len() >= 7 && t[..7].eq_ignore_ascii_case("chapter") {
        let mut i = 7;
        while i < t.len() && bytes[i] == b' ' {
            i += 1;
        }
        let digits_start = i;
        while i < t.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i > digits_start {
            while i < t.len() && bytes[i] == b' ' {
                i += 1;
            }
            // Skip one separator: ASCII '-'/':'/'|' or a Unicode en/em dash.
            if i < t.len() && (bytes[i] == b'-' || bytes[i] == b':' || bytes[i] == b'|') {
                i += 1;
            } else if let Some(c) = t[i..].chars().next() {
                if c == '\u{2013}' || c == '\u{2014}' {
                    i += c.len_utf8();
                }
            }
            while i < t.len() && bytes[i] == b' ' {
                i += 1;
            }
            let rest = t[i..].trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    t.to_string()
}

/// Extract a chapter number from a novgo-style chapter URL, e.g.
/// `/novel/chapter-42-some-title.html` -> `42`.
pub fn parse_chapter_number(url: &str) -> Option<u32> {
    let idx = url.find("chapter-")? + "chapter-".len();
    let digits: String = url[idx..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse a chapter selection like `"152"`, `"152-154"` or `"1,5,10-20"` into
/// the set of numbers it names.
///
/// Ranges are inclusive, because that is how a reader refers to chapters:
/// "152-154" means all three. Chapter numbering starts at 1, so 0 is rejected
/// rather than silently dropped — a spec that doesn't mean what the user typed
/// should fail loudly, since the commands built on this rewrite stored text.
pub fn parse_chapter_spec(spec: &str) -> Result<std::collections::BTreeSet<u32>, String> {
    let mut out = std::collections::BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Split on the *last* '-' so a leading minus reads as a bad number
        // rather than an open range.
        match part.split_once('-') {
            Some((lo, hi)) => {
                let lo: u32 = parse_number(lo)?;
                let hi: u32 = parse_number(hi)?;
                if lo > hi {
                    return Err(format!("range {part:?} counts backwards"));
                }
                out.extend(lo..=hi);
            }
            None => {
                out.insert(parse_number(part)?);
            }
        }
    }
    if out.is_empty() {
        return Err(format!("{spec:?} names no chapters"));
    }
    Ok(out)
}

fn parse_number(raw: &str) -> Result<u32, String> {
    let t = raw.trim();
    let n: u32 = t
        .parse()
        .map_err(|_| format!("{t:?} is not a chapter number"))?;
    if n == 0 {
        return Err("chapter numbers start at 1".to_string());
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_illegal_characters() {
        assert_eq!(sanitize_filename("A/B:C?D"), "A_B_C_D");
    }

    #[test]
    fn normalizes_titles_for_matching() {
        // Same novel, different site formatting -> equal.
        assert_eq!(normalize_title("Shadow Slave"), normalize_title("shadow-slave"));
        assert_eq!(normalize_title("Pain Immunity: X!"), "painimmunityx");
        // Distinct titles stay distinct.
        assert_ne!(normalize_title("Slime Evolution"), normalize_title("Slime Rancher"));
        // Non-Latin titles survive (Chinese chars are alphanumeric).
        assert_eq!(normalize_title("陷阵营营长"), "陷阵营营长");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_filename("Novel Name. "), "Novel Name");
    }

    #[test]
    fn escapes_reserved_names() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul.epub"), "_nul.epub");
    }

    #[test]
    fn never_empty() {
        assert_eq!(sanitize_filename("   "), "untitled");
        assert_eq!(sanitize_filename("???"), "untitled");
    }

    #[test]
    fn keeps_normal_titles() {
        assert_eq!(sanitize_filename("Cultivation Online"), "Cultivation Online");
    }

    #[test]
    fn cleans_chapter_title_prefix() {
        assert_eq!(
            clean_chapter_title("Chapter 1 - Cultivation Online"),
            "Cultivation Online"
        );
        assert_eq!(
            clean_chapter_title("Chapter 42: Death Penalty"),
            "Death Penalty"
        );
        assert_eq!(
            clean_chapter_title("Chapter 7 \u{2013} The Stone Tablets"),
            "The Stone Tablets"
        );
        // Only a prefix, no real name -> keep original.
        assert_eq!(clean_chapter_title("Chapter 5"), "Chapter 5");
        // Not a chapter-prefixed title -> unchanged.
        assert_eq!(clean_chapter_title("Prologue"), "Prologue");
    }

    #[test]
    fn parses_chapter_numbers() {
        assert_eq!(
            parse_chapter_number("/cultivation-online-novel/chapter-42-some-title.html"),
            Some(42)
        );
        assert_eq!(
            parse_chapter_number("https://novgo.net/x/chapter-1-cultivation-online.html"),
            Some(1)
        );
        assert_eq!(parse_chapter_number("/no-chapter-here/index.html"), None);
    }

    #[test]
    fn parses_chapter_specs() {
        let set = |v: &[u32]| v.iter().copied().collect::<std::collections::BTreeSet<u32>>();
        assert_eq!(parse_chapter_spec("152").unwrap(), set(&[152]));
        // Ranges are inclusive at both ends.
        assert_eq!(parse_chapter_spec("152-154").unwrap(), set(&[152, 153, 154]));
        assert_eq!(parse_chapter_spec("1,5,10-12").unwrap(), set(&[1, 5, 10, 11, 12]));
        // Whitespace and overlap are tolerated; the result is a set.
        assert_eq!(parse_chapter_spec(" 3 , 1-3 ").unwrap(), set(&[1, 2, 3]));
        // A single-number "range" is just that number.
        assert_eq!(parse_chapter_spec("7-7").unwrap(), set(&[7]));
    }

    #[test]
    fn rejects_specs_that_would_not_mean_what_was_typed() {
        for bad in ["", "  ", ",", "abc", "1-", "-5", "0", "0-3", "5-1", "1,x"] {
            assert!(
                parse_chapter_spec(bad).is_err(),
                "{bad:?} should be rejected, not silently reinterpreted"
            );
        }
    }
}
