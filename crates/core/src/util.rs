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

/// Format Unix seconds as a human-readable UTC timestamp `YYYY-MM-DD HH:MM:SSZ`,
/// without pulling in a date crate. Uses Howard Hinnant's civil-from-days.
pub fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
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
    fn formats_unix_utc() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00:00Z");
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(format_unix_utc(1_609_459_200), "2021-01-01 00:00:00Z");
        // 2024-02-29 12:34:56 UTC (leap day) = 1709210096
        assert_eq!(format_unix_utc(1_709_210_096), "2024-02-29 12:34:56Z");
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
