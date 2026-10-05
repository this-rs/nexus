//! HTML to Markdown, on `htmd` (pure Rust, html5ever underneath).
//!
//! Chosen by measurement on a corpus (docs/agent-tools-parity.md §6): of five pure-Rust
//! converters, `htmd` kept headings, nested and ordered lists, tables, fenced code with its
//! language, inline code, emphasis, entities and image alt text, and with scripts, styles
//! and the like skipped it left no page code in the text. What it does not do — and this
//! module adds — is turn relative links into absolute ones (a model that sees `[docs](/a)`
//! cannot follow it) and decode non-UTF-8 pages.

use htmd::{Element, HtmlToMarkdown};
use url::Url;

/// Tags whose content is never page text. `form` is deliberately absent: some frameworks
/// wrap the whole page in one.
const SKIPPED: &[&str] = &[
    "script", "style", "noscript", "iframe", "svg", "head", "template",
];

/// Converts `html` to Markdown; links and images are made absolute against `base`.
pub fn html_to_markdown(html: &str, base: &Url) -> String {
    let anchor_base = base.clone();
    let image_base = base.clone();
    let converter = HtmlToMarkdown::builder()
        .skip_tags(SKIPPED.to_vec())
        .add_handler(vec!["a"], move |element: Element| {
            anchor(&element, &anchor_base)
        })
        .add_handler(vec!["img"], move |element: Element| {
            image(&element, &image_base)
        })
        .build();
    let markdown = converter.convert(html).unwrap_or_default();
    tidy(&markdown)
}

fn attribute(element: &Element, name: &str) -> Option<String> {
    element
        .attrs
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| a.value.to_string())
}

/// Resolves `reference` against `base`; anything that cannot become an http(s) URL (a
/// `javascript:` link, a malformed one) is dropped rather than shown as a link.
fn absolute(reference: &str, base: &Url) -> Option<String> {
    let url = base.join(reference.trim()).ok()?;
    matches!(url.scheme(), "http" | "https" | "mailto").then(|| url.to_string())
}

fn escape_parens(url: &str) -> String {
    url.replace('(', "%28")
        .replace(')', "%29")
        .replace(' ', "%20")
}

fn anchor(element: &Element, base: &Url) -> Option<String> {
    let text = element.content.trim();
    let Some(href) = attribute(element, "href") else {
        return Some(element.content.to_owned());
    };
    // A fragment-only link points inside the page: it is text, not a destination.
    if href.starts_with('#') {
        return Some(element.content.to_owned());
    }
    match absolute(&href, base) {
        Some(url) if text.is_empty() => Some(format!("<{url}>")),
        Some(url) => Some(format!("[{text}]({})", escape_parens(&url))),
        None => Some(element.content.to_owned()),
    }
}

fn image(element: &Element, base: &Url) -> Option<String> {
    let src = attribute(element, "src")?;
    let url = absolute(&src, base)?;
    let alt = attribute(element, "alt").unwrap_or_default();
    let alt = alt.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(format!("![{alt}]({})", escape_parens(&url)))
}

/// Trailing spaces and runs of blank lines carry no meaning in Markdown.
fn tidy(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut blank = 0;
    for line in markdown.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

/// Decodes a page body: the charset of the `Content-Type` header, else a `<meta charset>` in
/// the first kilobyte, else UTF-8 (invalid sequences become U+FFFD, never an error).
pub fn decode(body: &[u8], content_type: Option<&str>) -> String {
    let from_header = content_type.and_then(charset_of);
    let label = from_header.or_else(|| sniff_meta_charset(body));
    let encoding = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(body).0.into_owned()
}

fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let (name, value) = part.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn sniff_meta_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]).to_ascii_lowercase();
    let at = head.find("charset=")? + "charset=".len();
    let rest = head[at..].trim_start_matches(['"', '\'']);
    let end =
        rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':'))?;
    Some(rest[..end].to_owned())
}
