//! `Read` of a PDF: its text, by page range, in pure Rust (`lopdf`) (N19b).
//!
//! Text only: a scanned PDF (pictures of pages) has none, and says so. A long PDF must be read
//! in ranges, 20 pages at most at a time, so that one call cannot pour a book into the context.

use std::collections::BTreeSet;
use std::path::Path;

/// Pages returned by one call.
pub const MAX_PAGES_PER_CALL: usize = 20;
/// Pages a PDF may have before `pages` becomes mandatory.
pub const WHOLE_READ_LIMIT: usize = 10;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Parses `3`, `1-5`, `2,4-6` into page numbers (1-based).
pub fn parse_pages(spec: &str) -> Result<BTreeSet<u32>, String> {
    let bad = || format!("`pages` is not a page range: `{spec}` (use \"3\", \"1-5\" or \"2,4-6\")");
    let mut out = BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        let (from, to) = match part.split_once('-') {
            Some((a, b)) => (
                a.trim().parse::<u32>().map_err(|_| bad())?,
                b.trim().parse::<u32>().map_err(|_| bad())?,
            ),
            None => {
                let n = part.parse::<u32>().map_err(|_| bad())?;
                (n, n)
            },
        };
        if from == 0 || to < from {
            return Err(bad());
        }
        if (to - from) as usize >= MAX_PAGES_PER_CALL * 50 {
            return Err(bad());
        }
        out.extend(from..=to);
    }
    Ok(out)
}

pub fn read(path: &Path, pages: Option<&str>) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "This PDF is larger than {} MiB and will not be read.",
            MAX_FILE_BYTES / 1024 / 1024
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let document = lopdf::Document::load_mem(&bytes)
        .map_err(|_| "This file is not a PDF that can be read (it may be damaged).".to_owned())?;
    if document.is_encrypted() {
        return Err("This PDF is encrypted and cannot be read.".to_owned());
    }
    let all: Vec<u32> = document.get_pages().keys().copied().collect();
    let total = all.len();
    let wanted: Vec<u32> = match pages {
        Some(spec) => {
            let asked = parse_pages(spec)?;
            let kept: Vec<u32> = asked.into_iter().filter(|p| all.contains(p)).collect();
            if kept.is_empty() {
                return Err(format!(
                    "This PDF has {total} pages; none of `{spec}` exists."
                ));
            }
            kept
        },
        None if total > WHOLE_READ_LIMIT => {
            return Err(format!(
                "This PDF has {total} pages, which is too many to read at once. Use the `pages` parameter to read a range (at most {MAX_PAGES_PER_CALL} pages per request), for example pages: \"1-5\"."
            ));
        },
        None => all,
    };
    if wanted.len() > MAX_PAGES_PER_CALL {
        return Err(format!(
            "{} pages were asked for; at most {MAX_PAGES_PER_CALL} can be read per request.",
            wanted.len()
        ));
    }
    let mut out = String::new();
    let mut any_text = false;
    for page in &wanted {
        let text = document.extract_text(&[*page]).unwrap_or_default();
        let text = text.trim();
        any_text |= !text.is_empty();
        out.push_str(&format!("--- page {page} of {total} ---\n{text}\n"));
    }
    if !any_text {
        out.push_str("\n(no text was found: this PDF may be made of scanned images)\n");
    }
    Ok(out.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ranges_parse_and_bad_ones_are_refused() {
        assert_eq!(parse_pages("3").unwrap(), BTreeSet::from([3]));
        assert_eq!(parse_pages("1-3").unwrap(), BTreeSet::from([1, 2, 3]));
        assert_eq!(parse_pages("2, 4-5").unwrap(), BTreeSet::from([2, 4, 5]));
        for bad in ["", "0", "a", "3-1", "1-", "-2", "1,,2", "1-99999999"] {
            assert!(parse_pages(bad).is_err(), "{bad:?}");
        }
    }
}
