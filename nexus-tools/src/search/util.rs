//! Small helpers the engines share (N22).

use crate::web::WebError;

use super::backend::SearchError;

/// What a failed request means for a search engine.
pub fn from_web(error: WebError, keyed: bool) -> SearchError {
    match error {
        WebError::Timeout => SearchError::Timeout,
        WebError::HttpStatus { status, .. } => match status {
            401 | 403 if keyed => SearchError::KeyRejected,
            402 => SearchError::QuotaExceeded,
            429 => SearchError::RateLimited,
            s => SearchError::Unavailable(format!("HTTP {s}")),
        },
        WebError::BlockedAddress { host, .. } => SearchError::NotConfigured(format!(
            "{host} is a private address; enable allow_private_network for this engine if it is yours"
        )),
        other => SearchError::Unavailable(other.kind().to_owned()),
    }
}

/// Removes tags and decodes the common entities: engines put `<strong>` around matches.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            },
            _ if !in_tag => out.push(c),
            _ => {},
        }
    }
    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `&amp; &lt; &gt; &quot; &#39; &#x27; &nbsp;` and numeric references.
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match replacement {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            },
            None => {
                out.push('&');
                rest = &tail[1..];
            },
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_go_and_entities_are_decoded() {
        assert_eq!(
            plain_text("a <strong>bold</strong> &amp; <em>more</em>"),
            "a bold & more"
        );
        assert_eq!(
            plain_text("x&nbsp;y &lt;tag&gt; &#39;q&#x27; &quot;z&quot;"),
            "x y <tag> 'q' \"z\""
        );
        assert_eq!(
            plain_text("AT&T &unknown; & alone"),
            "AT&T &unknown; & alone"
        );
        assert_eq!(plain_text("<script>alert(1)</script>text"), "alert(1) text");
    }

    #[test]
    fn statuses_map_to_what_they_mean() {
        let status = |s| WebError::HttpStatus {
            status: s,
            url: "u".into(),
        };
        assert_eq!(from_web(status(401), true), SearchError::KeyRejected);
        assert_eq!(from_web(status(403), true), SearchError::KeyRejected);
        assert_eq!(from_web(status(402), true), SearchError::QuotaExceeded);
        assert_eq!(from_web(status(429), true), SearchError::RateLimited);
        assert_eq!(
            from_web(status(500), true),
            SearchError::Unavailable("HTTP 500".into())
        );
        // Without a key, a 403 is not about a key.
        assert_eq!(
            from_web(status(403), false),
            SearchError::Unavailable("HTTP 403".into())
        );
        assert_eq!(from_web(WebError::Timeout, true), SearchError::Timeout);
    }
}
