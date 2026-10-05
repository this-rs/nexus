//! HTML to Markdown on the measured corpus (N21): what the chosen converter must keep and
//! what it must never let through. `tests/data/html/` is the corpus of the measurement
//! recorded in docs/agent-tools-parity.md §6.

use nexus_tools::web::{decode, html_to_markdown};
use url::Url;

fn page(name: &str) -> String {
    let path = format!("{}/tests/data/html/{name}.html", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn convert(name: &str) -> String {
    html_to_markdown(
        &page(name),
        &Url::parse("https://site.test/docs/index.html").unwrap(),
    )
}

#[test]
fn structure_is_kept() {
    let md = convert("article");
    for want in [
        "# Main Title",
        "## Section Two",
        "### Sub",
        "**bold**",
        "`inline_code()`",
        "apple",
        "nested one",
        "nested two",
        "cherry",
        "1.  first",
        "2.  second",
        "> A quoted line.",
        "& entity <tag>",
        "©",
    ] {
        assert!(md.contains(want), "missing {want:?} in:\n{md}");
    }
}

#[test]
fn page_code_and_chrome_never_reach_the_text() {
    let md = convert("article");
    for banned in ["SCRIPTBODY", "color:red", "var secret"] {
        assert!(!md.contains(banned), "{banned:?} leaked into:\n{md}");
    }
    let messy = convert("messy");
    for banned in [
        "INJECTED",
        "COMMENTBODY",
        "evil.test",
        "SVGTEXT",
        "document.write",
    ] {
        assert!(!messy.contains(banned), "{banned:?} leaked into:\n{messy}");
    }
    // The <title> is in <head>, which is skipped: no stray first line.
    assert!(!md.contains("Doc Title"));
}

#[test]
fn links_and_images_become_absolute_and_unsafe_ones_become_text() {
    let md = convert("article");
    assert!(md.contains("[link text](https://x.test/a)"), "{md}");
    assert!(
        md.contains("[relative link](https://site.test/rel/path?q=1)"),
        "{md}"
    );
    assert!(md.contains("![An image](https://x.test/i.png)"), "{md}");
    assert!(md.contains("![Local](https://site.test/local.png)"), "{md}");
    // An in-page anchor and a javascript: URL are not destinations: only their text remains.
    assert!(
        md.contains("in-page link") && !md.contains("(#frag)"),
        "{md}"
    );
    assert!(
        md.contains("script link") && !md.contains("javascript:"),
        "{md}"
    );
}

#[test]
fn tables_become_markdown_tables() {
    let md = convert("table");
    assert!(md.contains("## Prices"));
    let lines: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
    assert_eq!(lines.len(), 4, "{md}");
    assert!(lines[0].contains("Item") && lines[0].contains("Price"));
    assert!(
        lines[1].chars().all(|c| matches!(c, '|' | '-' | ' ' | ':')),
        "separator row: {}",
        lines[1]
    );
    assert!(lines[2].contains("Pen") && lines[2].contains("1.50"));
    assert!(lines[3].contains("Book") && lines[3].contains("12.00"));
}

#[test]
fn code_blocks_keep_their_language_and_decode_entities() {
    let md = convert("code");
    assert!(md.contains("```rust\nfn main() {"), "{md}");
    assert!(md.contains("println!(\"a < b && c\");"), "{md}");
    assert!(!md.contains("&lt;") && !md.contains("&amp;"));
    assert!(md.contains("`cargo test`"));
    assert!(md.contains("plain pre") && md.contains("indented"));
}

#[test]
fn a_page_wrapped_in_a_form_is_not_lost() {
    let md = convert("wrapped_in_form");
    for want in [
        "# Whole page in a form",
        "Content that must survive.",
        "kept",
    ] {
        assert!(md.contains(want), "missing {want:?}:\n{md}");
    }
}

#[test]
fn broken_html_still_gives_text() {
    let md = convert("messy");
    for want in [
        "# Upper",
        "unclosed paragraph",
        "second",
        "bold",
        "one",
        "two",
    ] {
        assert!(md.contains(want), "missing {want:?}:\n{md}");
    }
    assert_eq!(
        html_to_markdown("", &Url::parse("https://a.test/").unwrap()),
        ""
    );
    assert!(html_to_markdown("<<<>>> <p", &Url::parse("https://a.test/").unwrap()).contains("<<<"));
}

#[test]
fn no_run_of_blank_lines_and_no_trailing_spaces() {
    let md = convert("article");
    assert!(!md.contains("\n\n\n"), "{md}");
    assert!(md.lines().all(|l| l == l.trim_end()));
}

#[test]
fn bodies_are_decoded_with_their_declared_charset() {
    // 0xE9 is é in ISO-8859-1 and an invalid byte in UTF-8.
    let latin1 = b"caf\xe9";
    assert_eq!(
        decode(latin1, Some("text/html; charset=ISO-8859-1")),
        "café"
    );
    assert_eq!(
        decode(latin1, Some("text/html; charset=\"latin1\"")),
        "café"
    );
    assert_eq!(
        decode(b"<meta charset=windows-1252>caf\xe9", None),
        "<meta charset=windows-1252>café"
    );
    assert_eq!(decode("café".as_bytes(), Some("text/html")), "café");
    // An invalid sequence is replaced, never an error.
    assert_eq!(decode(b"a\xffb", Some("text/plain")), "a\u{fffd}b");
    // An unknown label falls back to UTF-8.
    assert_eq!(
        decode("ok".as_bytes(), Some("text/plain; charset=nonsense")),
        "ok"
    );
}
