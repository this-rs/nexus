//! `covers` globs, matched against repository paths.
//!
//! Matching is done on the PATH, never by listing the disk: a file a pull
//! request deletes is still a change to what its diagram describes, and a
//! directory listing cannot see it.

/// Does `pattern` match the repository path `path`?
///
/// `**/` spans any number of directories (none included), a bare `**` spans
/// anything, `*` and `?` stay inside one path segment. Everything else is
/// literal.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let path: Vec<char> = path.chars().collect();
    walk(&pattern, &path)
}

fn walk(pattern: &[char], path: &[char]) -> bool {
    match pattern {
        [] => path.is_empty(),
        ['*', '*', '/', rest @ ..] => {
            walk(rest, path)
                || (0..path.len()).any(|i| path[i] == '/' && walk(rest, &path[i + 1..]))
        },
        ['*', '*', rest @ ..] => (0..=path.len()).any(|i| walk(rest, &path[i..])),
        ['*', rest @ ..] => {
            let segment = path.iter().position(|c| *c == '/').unwrap_or(path.len());
            (0..=segment).any(|i| walk(rest, &path[i..]))
        },
        ['?', rest @ ..] => matches!(path, [c, tail @ ..] if *c != '/' && walk(rest, tail)),
        [literal, rest @ ..] => matches!(path, [c, tail @ ..] if c == literal && walk(rest, tail)),
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn star_stays_inside_one_segment() {
        assert!(matches(
            "sdk/src/transport/*.rs",
            "sdk/src/transport/mod.rs"
        ));
        assert!(!matches(
            "sdk/src/transport/*.rs",
            "sdk/src/transport/deep/mod.rs"
        ));
        assert!(!matches(
            "sdk/src/transport/*.rs",
            "sdk/src/transport/mod.rs.bak"
        ));
        assert!(matches("a/*", "a/"));
    }

    #[test]
    fn double_star_slash_spans_zero_or_more_directories() {
        assert!(matches("api/src/**/*.rs", "api/src/lib.rs"));
        assert!(matches("api/src/**/*.rs", "api/src/a/b/c.rs"));
        assert!(!matches("api/src/**/*.rs", "other/api/src/lib.rs"));
        assert!(!matches("api/src/**/*.rs", "api/srcx/lib.rs"));
    }

    #[test]
    fn bare_double_star_spans_anything() {
        assert!(matches("api/**", "api/x/y.toml"));
        assert!(matches("api/**", "api/"));
        assert!(!matches("api/**", "apx/y"));
    }

    #[test]
    fn question_mark_is_one_character_of_a_segment() {
        assert!(matches("a/?.rs", "a/b.rs"));
        assert!(matches("a/?.rs", "a/é.rs"));
        assert!(!matches("a/?.rs", "a/bc.rs"));
        assert!(!matches("a/?.rs", "a//.rs"));
        assert!(!matches("a/?", "a/"));
    }

    #[test]
    fn everything_else_is_literal() {
        assert!(matches("a/b.rs", "a/b.rs"));
        assert!(!matches("a/b.rs", "a/bxrs"));
        assert!(!matches("a/b.rs", "a/b.rsx"));
        assert!(!matches("a/b.rs", "a/b.r"));
        assert!(matches("", ""));
    }
}
