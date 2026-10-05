//! Output caps. A cut is always **said**: a model that reads a silently truncated result
//! reasons about a file or a command output that does not exist.

/// What the server keeps of one tool result by default.
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 100_000;

/// Keeps at most `max` characters of `text` and, when something was cut, ends it with a
/// marker that says how much. Never splits a character.
pub fn truncate(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!(
        "{kept}\n[output truncated: {} characters omitted of {total}]",
        total - max
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_text_is_untouched() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn a_cut_is_said_with_the_amount_omitted() {
        let cut = truncate("abcdefghij", 4);
        assert!(cut.starts_with("abcd\n"), "{cut}");
        assert!(cut.contains("6 characters omitted of 10"), "{cut}");
    }

    #[test]
    fn it_never_splits_a_character() {
        let text = "ééééé";
        let cut = truncate(text, 2);
        assert!(cut.starts_with("éé\n[output truncated"), "{cut}");
    }
}
