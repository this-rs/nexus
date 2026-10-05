//! Canonical URLs (to merge duplicates) and domain filters (applied after the engine, never
//! trusting it) (N22).

use url::Url;

/// Query parameters that identify a visit, not a page.
const TRACKING: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid", "yclid", "mc_cid", "mc_eid", "igshid", "ref",
    "ref_src", "ref_url", "_ga", "_gl", "spm", "cmpid", "ocid", "wt_mc", "mkt_tok", "vero_id",
    "s_cid",
];

fn is_tracking(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("utm_") || TRACKING.contains(&lower.as_str())
}

/// A form of `url` that is equal for equivalent addresses: scheme ignored (`http` and `https`),
/// `www.` ignored, host lowercased, default port, fragment and tracking parameters dropped,
/// remaining parameters sorted, trailing slash dropped. `None` when it is not an http(s) URL.
pub fn canonical(url: &str) -> Option<String> {
    let parsed = Url::parse(url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed
        .host_str()?
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    let mut params: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(name, _)| !is_tracking(name))
        .map(|(n, v)| (n.into_owned(), v.into_owned()))
        .collect();
    params.sort();
    let path = parsed.path().trim_end_matches('/');
    let mut out = host;
    if let Some(port) = parsed.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(path);
    if !params.is_empty() {
        let query: Vec<String> = params.iter().map(|(n, v)| format!("{n}={v}")).collect();
        out.push('?');
        out.push_str(&query.join("&"));
    }
    Some(out)
}

/// `url` as shown to the model: the same address without the fragment and the tracking
/// parameters (they identify a visit, not a page, and cost tokens). Anything else is untouched.
pub fn tidy(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url.trim()) else {
        return url.trim().to_owned();
    };
    parsed.set_fragment(None);
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(name, _)| !is_tracking(name))
        .map(|(n, v)| (n.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.query_pairs_mut().clear().extend_pairs(kept);
    }
    parsed.to_string()
}

/// The host of `url`, lowercased, without `www.`.
fn host_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url.trim()).ok()?;
    parsed
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
}

/// Whether `host` is `domain` or a subdomain of it. `notexample.com` is not `example.com`.
fn in_domain(host: &str, domain: &str) -> bool {
    let domain = domain
        .trim()
        .trim_start_matches("www.")
        .trim_start_matches('.')
        .to_ascii_lowercase();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

/// Whether a result at `url` may be shown: not under a blocked domain, and — when allowed
/// domains are given — under one of them. A URL that cannot be parsed is never shown.
pub fn permitted(url: &str, allowed: &[String], blocked: &[String]) -> bool {
    let Some(host) = host_of(url) else {
        return false;
    };
    if blocked.iter().any(|d| in_domain(&host, d)) {
        return false;
    }
    allowed.is_empty() || allowed.iter().any(|d| in_domain(&host, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_addresses_have_one_canonical_form() {
        let same = [
            "https://example.com/a/b",
            "http://example.com/a/b",
            "https://www.example.com/a/b",
            "HTTPS://Example.COM/a/b/",
            "https://example.com/a/b#section",
            "https://example.com/a/b?utm_source=x&utm_medium=y",
            "https://example.com:443/a/b",
            "https://example.com/a/b?fbclid=abc&gclid=1",
        ];
        let first = canonical(same[0]).unwrap();
        for url in same {
            assert_eq!(canonical(url).as_deref(), Some(first.as_str()), "{url}");
        }
        assert_eq!(first, "example.com/a/b");
    }

    #[test]
    fn different_pages_stay_different() {
        let a = canonical("https://example.com/a?id=1").unwrap();
        for other in [
            "https://example.com/a?id=2",
            "https://example.com/b?id=1",
            "https://example.org/a?id=1",
            "https://sub.example.com/a?id=1",
            "https://example.com:8080/a?id=1",
        ] {
            assert_ne!(canonical(other).unwrap(), a, "{other}");
        }
        // Parameter order does not matter; a meaningful parameter does.
        assert_eq!(
            canonical("https://e.com/?a=1&b=2"),
            canonical("https://e.com/?b=2&a=1")
        );
        assert_ne!(
            canonical("https://e.com/?a=1"),
            canonical("https://e.com/?a=2")
        );
    }

    #[test]
    fn what_is_not_a_web_address_has_no_canonical_form() {
        for url in [
            "",
            "not a url",
            "javascript:alert(1)",
            "ftp://e.com/",
            "mailto:a@b.c",
            "file:///etc/passwd",
        ] {
            assert_eq!(canonical(url), None, "{url}");
        }
    }

    #[test]
    fn a_displayed_address_loses_tracking_and_the_fragment_and_nothing_else() {
        assert_eq!(
            tidy("https://www.example.com/a?utm_source=x&id=7&fbclid=z#top"),
            "https://www.example.com/a?id=7"
        );
        assert_eq!(
            tidy("http://example.com/a/?utm_medium=m"),
            "http://example.com/a/"
        );
        assert_eq!(
            tidy("https://example.com/p?q=a+b&x=1"),
            "https://example.com/p?q=a+b&x=1"
        );
        assert_eq!(tidy("not a url"), "not a url");
    }

    #[test]
    fn a_domain_matches_itself_and_its_subdomains_only() {
        let blocked = vec!["example.com".to_owned()];
        for url in [
            "https://example.com/x",
            "https://www.example.com/x",
            "https://a.b.example.com/x",
            "http://EXAMPLE.com/",
        ] {
            assert!(!permitted(url, &[], &blocked), "{url} should be blocked");
        }
        for url in [
            "https://notexample.com/x",
            "https://example.com.evil.test/x",
            "https://example.org/x",
        ] {
            assert!(permitted(url, &[], &blocked), "{url} should pass");
        }
    }

    #[test]
    fn allowed_domains_restrict_and_blocked_wins() {
        let allowed = vec!["rust-lang.org".to_owned(), "docs.rs".to_owned()];
        assert!(permitted("https://doc.rust-lang.org/std", &allowed, &[]));
        assert!(permitted("https://docs.rs/serde", &allowed, &[]));
        assert!(!permitted("https://example.com/", &allowed, &[]));
        let blocked = vec!["doc.rust-lang.org".to_owned()];
        assert!(
            !permitted("https://doc.rust-lang.org/std", &allowed, &blocked),
            "blocked wins over allowed"
        );
        assert!(permitted("https://www.rust-lang.org/", &allowed, &blocked));
    }

    #[test]
    fn an_unparsable_result_is_never_shown() {
        assert!(!permitted("", &[], &[]));
        assert!(!permitted("not a url", &[], &[]));
    }
}
