//! The three `%% key: value` lines every diagram starts with.
//!
//! ```text
//! %% name: nexus-sdk-transport
//! %% covers: claude-code-sdk-rs/src/transport/*.rs
//! %% verified: 2026-10-01
//! ```
//!
//! The diagrams are the single source of truth for which source files each one
//! owns: the index is generated from these lines, never written by hand.

/// A well-formed header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    /// One or more globs, comma-separated in the file.
    pub covers: Vec<String>,
    pub verified: String,
}

/// How many leading lines may carry the header.
const HEADER_LINES: usize = 6;

/// Reads the header of the diagram at repository path `path`.
///
/// Returns the header when nothing is wrong with it, and every problem
/// otherwise: a missing or empty key, a `verified` that cannot be replayed, a
/// `name` that is not the file name (the index keys on it).
pub fn parse(path: &str, text: &str) -> (Option<Header>, Vec<String>) {
    let (mut name, mut covers, mut verified) = (String::new(), Vec::new(), String::new());
    for line in text.lines().take(HEADER_LINES) {
        let Some((key, value)) = line.strip_prefix("%% ").and_then(|l| l.split_once(':')) else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" => name = value.to_owned(),
            "verified" => verified = value.to_owned(),
            "covers" => {
                covers = value
                    .split(',')
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(str::to_owned)
                    .collect();
            },
            _ => {},
        }
    }

    let mut problems = Vec::new();
    for (key, empty) in [
        ("name", name.is_empty()),
        ("covers", covers.is_empty()),
        ("verified", verified.is_empty()),
    ] {
        if empty {
            problems.push(format!("{path}: header `{key}` is missing or empty"));
        }
    }
    if !verified.is_empty()
        && let Some(why) = verified_problem(&verified)
    {
        problems.push(format!("{path}: header `verified`: {why}"));
    }
    let stem = path.rsplit('/').next().unwrap_or(path);
    let stem = stem.strip_suffix(".mmd").unwrap_or(stem);
    if !name.is_empty() && name != stem {
        problems.push(format!(
            "{path}: name `{name}` does not match the file name"
        ));
    }

    if problems.is_empty() {
        (
            Some(Header {
                name,
                covers,
                verified,
            }),
            problems,
        )
    } else {
        (None, problems)
    }
}

/// Why a `%% verified:` value cannot be replayed, or `None` when it can.
///
/// `verified` records WHAT the author checked the diagram against, so that a
/// reviewer can look at the same thing: a calendar date (`git log --until`) or
/// a git sha (`git show <sha>:<file>`). `yesterday` and `TODO` cannot be
/// replayed, which makes the diagram unverifiable — the very defect diagrams
/// are meant to remove.
pub fn verified_problem(value: &str) -> Option<String> {
    let hex = value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
    if hex && (7..=40).contains(&value.len()) {
        return None;
    }
    match date_shape(value) {
        Some((year, month, day)) if is_calendar_day(year, month, day) => None,
        Some(_) => Some(format!(
            "`{value}` has the shape of a date but is not a calendar day"
        )),
        None => Some(format!(
            "`{value}` is neither an ISO date (YYYY-MM-DD) nor a short git sha — it cannot be replayed"
        )),
    }
}

/// `YYYY-MM-DD` as numbers, when `value` has exactly that shape.
fn date_shape(value: &str) -> Option<(u32, u32, u32)> {
    let bytes = value.as_bytes();
    let shaped = bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    if !shaped {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value[range].parse::<u32>().ok();
    Some((number(0..4)?, number(5..7)?, number(8..10)?))
}

fn is_calendar_day(year: u32, month: u32, day: u32) -> bool {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 1 && (1..=days).contains(&day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problems(verified: &str) -> Vec<String> {
        let text =
            format!("%% name: demo\n%% covers: a/*.rs\n%% verified: {verified}\nflowchart TD\n");
        parse("docs/diagrams/demo.mmd", &text).1
    }

    #[test]
    fn a_complete_header_is_read() {
        let text =
            "%% name: demo\n%% covers: a/*.rs,  b/c.rs ,\n%% verified: 443a2a0\nflowchart TD\n";
        let (header, problems) = parse("docs/diagrams/demo.mmd", text);
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(
            header,
            Some(Header {
                name: "demo".into(),
                covers: vec!["a/*.rs".into(), "b/c.rs".into()],
                verified: "443a2a0".into(),
            })
        );
    }

    #[test]
    fn crlf_line_endings_are_read_the_same() {
        let text =
            "%% name: demo\r\n%% covers: a/*.rs\r\n%% verified: 2026-10-02\r\nflowchart TD\r\n";
        assert!(parse("demo.mmd", text).0.is_some());
    }

    #[test]
    fn iso_date_is_accepted() {
        assert_eq!(problems("2026-10-02"), Vec::<String>::new());
        assert_eq!(problems("2024-02-29"), Vec::<String>::new());
    }

    #[test]
    fn short_and_full_git_sha_are_accepted() {
        assert_eq!(problems("443a2a0"), Vec::<String>::new());
        assert_eq!(
            problems(&format!("443a2a0{}", "b".repeat(33))),
            Vec::<String>::new()
        );
    }

    #[test]
    fn yesterday_is_rejected() {
        let found = problems("yesterday");
        assert_eq!(found.len(), 1);
        assert!(
            found[0].contains("verified") && found[0].contains("yesterday"),
            "{found:?}"
        );
    }

    #[test]
    fn todo_is_rejected() {
        assert_eq!(problems("TODO").len(), 1);
    }

    #[test]
    fn an_impossible_date_is_rejected() {
        for value in [
            "2026-13-45",
            "2026-02-29",
            "2026-04-31",
            "2026-00-10",
            "2026-01-00",
            "0000-01-01",
            "1900-02-29",
        ] {
            let found = problems(value);
            assert_eq!(found.len(), 1, "{value}");
            assert!(found[0].contains("not a calendar day"), "{found:?}");
        }
        assert_eq!(problems("2000-02-29"), Vec::<String>::new());
    }

    #[test]
    fn too_short_too_long_or_non_hex_sha_is_rejected() {
        assert_eq!(problems("abc12").len(), 1);
        assert_eq!(problems("zzzzzzz").len(), 1);
        assert_eq!(problems("ABCDEF1").len(), 1);
        assert_eq!(problems(&"a".repeat(41)).len(), 1);
        assert_eq!(problems("2026/10/02").len(), 1);
    }

    #[test]
    fn each_missing_key_is_reported() {
        let (header, found) = parse("docs/diagrams/demo.mmd", "flowchart TD\n%% not a header\n");
        assert_eq!(header, None);
        assert_eq!(found.len(), 3);
        for key in ["name", "covers", "verified"] {
            assert!(
                found
                    .iter()
                    .any(|p| p.contains(&format!("`{key}` is missing"))),
                "{key}"
            );
        }
    }

    #[test]
    fn an_empty_covers_and_an_unknown_key_are_handled() {
        let text = "%% name: demo\n%% covers: ,\n%% owner: someone\n%% verified: 2026-10-02\n";
        let found = parse("demo.mmd", text).1;
        assert_eq!(
            found,
            vec!["demo.mmd: header `covers` is missing or empty".to_owned()]
        );
    }

    #[test]
    fn a_header_below_the_sixth_line_is_not_a_header() {
        let text = "\n\n\n\n\n\n%% name: demo\n%% covers: a\n%% verified: 2026-10-02\n";
        assert_eq!(parse("demo.mmd", text).1.len(), 3);
    }

    #[test]
    fn name_must_be_the_file_name() {
        let text = "%% name: other\n%% covers: a/*.rs\n%% verified: 2026-10-02\n";
        let found = parse("docs/diagrams/demo.mmd", text).1;
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("does not match the file name"));
    }
}
