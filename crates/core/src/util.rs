//! Small cross-cutting helpers.

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

/// Strip a leading "Chapter N -/–/:" prefix from a table-of-contents link's
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
            // Skip one separator: ASCII '-'/':' or a Unicode en/em dash.
            if i < t.len() && (bytes[i] == b'-' || bytes[i] == b':') {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_illegal_characters() {
        assert_eq!(sanitize_filename("A/B:C?D"), "A_B_C_D");
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
}
