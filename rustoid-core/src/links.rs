//! Link handling — wikilinks, interwiki links, external links.
//!
//! Handles parsing and resolution of `[[...]]` wikilinks,
//! interwiki prefixes, and `[http://...]` external links.

use crate::title::Title;
use crate::traits::SiteConfig;

/// Link target information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An internal wikilink.
    Wikilink {
        title: Title,
        fragment: Option<String>,
    },
    /// An interwiki link (prefix + title on another wiki).
    Interwiki { prefix: String, title: String },
    /// An external URL link.
    ExtLink { url: String },
}

/// Parse a wikilink target (the content inside `[[...]]`).
///
/// Returns the link target and optional display text.
pub fn parse_wikilink(_raw: &str, _config: &dyn SiteConfig) -> (LinkTarget, Option<String>) {
    // Placeholder — will be implemented in Phase 2/3
    (
        LinkTarget::Wikilink {
            title: Title::new_main(""),
            fragment: None,
        },
        None,
    )
}

/// Parse an external link (`[http://example.com text]`).
pub fn parse_extlink(url: &str, text: &str) -> (String, Option<String>) {
    let url = url.trim().to_string();
    let display = if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    };
    (url, display)
}

/// `UrlUtils::matchesDomainList` — whether `url`'s host ends with one of
/// `domains` at a label boundary.
///
/// Mirrors PHP by prepending `.` to the host and to every domain before the
/// suffix test, so `notwikimedia.org` does not match `wikimedia.org`. Used for
/// `$wgNoFollowDomainExceptions`.
pub fn url_host_matches_domain_list(url: &str, domains: &[&str]) -> bool {
    let Some(host) = url_host(url) else {
        return false;
    };
    let host = format!(".{host}");
    domains.iter().any(|d| {
        let d = d.trim_start_matches('.');
        !d.is_empty() && host.ends_with(&format!(".{d}"))
    })
}

/// Expand a protocol-relative URL (`//host/…`) that names the wiki's own server
/// to its absolute form, returning `None` for any other URL.
///
/// A `{{fullurl:…}}` answer is protocol-relative (`Title::getFullURL` expands
/// with `PROTO_RELATIVE`), and Wikimedia rewrites such a link *to the wiki
/// itself* to the request protocol, keeping the written form in
/// `data-mw-original-href` (`Env::getExternalLinkAttribs`, whose `href` differs
/// from the input when the wiki's `getExternalUrlInfo` rewrites it). A foreign
/// host is left protocol-relative — probed on the live wiki: `[//example.com/x
/// y]` keeps `//example.com/x`, while `[//en.wikipedia.org/wiki/Z w]` becomes
/// `https://en.wikipedia.org/wiki/Z` with the original in `data-mw-original-href`.
pub fn protocol_expand_same_host(href: &str, server_url: &str) -> Option<String> {
    let rest = href.strip_prefix("//")?;
    let site_host = url_host(server_url)?;
    if !url_host(href)?.eq_ignore_ascii_case(site_host) {
        return None;
    }
    let protocol = server_url
        .split_once("://")
        .map(|(protocol, _)| protocol)
        .unwrap_or("https");
    Some(format!("{protocol}://{rest}"))
}

/// The host of an absolute or protocol-relative URL (PHP's `parse_url` host).
/// Returns `None` for a URL without one (`mailto:`, relative paths).
fn url_host(url: &str) -> Option<&str> {
    let rest = match url.strip_prefix("//") {
        Some(r) => r,
        None => {
            let idx = url.find("://")?;
            &url[idx + 3..]
        }
    };
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].split(':').next().unwrap_or("");
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::protocol_expand_same_host;
    use super::url_host_matches_domain_list;

    #[test]
    fn protocol_relative_same_host_expands() {
        // A protocol-relative URL to the wiki's own server gets the site
        // protocol; a foreign host is left alone. Probed on the live wiki.
        assert_eq!(
            protocol_expand_same_host("//en.wikipedia.org/wiki/Z", "//en.wikipedia.org"),
            Some("https://en.wikipedia.org/wiki/Z".to_string())
        );
        assert_eq!(
            protocol_expand_same_host("//en.wikipedia.org/wiki/Z", "https://en.wikipedia.org"),
            Some("https://en.wikipedia.org/wiki/Z".to_string())
        );
        assert_eq!(
            protocol_expand_same_host("//example.com/x", "//en.wikipedia.org"),
            None
        );
        // Already absolute, or not a URL with a host.
        assert_eq!(
            protocol_expand_same_host("https://en.wikipedia.org/x", "//en.wikipedia.org"),
            None
        );
        assert_eq!(
            protocol_expand_same_host("/wiki/X", "//en.wikipedia.org"),
            None
        );
    }

    #[test]
    fn a_host_matches_a_domain_at_a_label_boundary() {
        let domains = ["wikimedia.org", "wikipedia.org"];
        // The domain itself, a subdomain, a port, and a scheme-less form all match.
        assert!(url_host_matches_domain_list(
            "https://wikipedia.org/",
            &domains
        ));
        assert!(url_host_matches_domain_list(
            "https://en.wikipedia.org/wiki/Foo",
            &domains
        ));
        assert!(url_host_matches_domain_list(
            "https://en.wikipedia.org:443/x",
            &domains
        ));
        assert!(url_host_matches_domain_list(
            "//en.wikipedia.org/x",
            &domains
        ));
    }

    #[test]
    fn a_non_boundary_suffix_does_not_match() {
        // `notwikipedia.org` ends with `wikipedia.org` but not at a label
        // boundary; the prepended `.` is what rejects it.
        let domains = ["wikipedia.org"];
        assert!(!url_host_matches_domain_list(
            "https://notwikipedia.org/",
            &domains
        ));
        // Userinfo is not the host.
        assert!(!url_host_matches_domain_list(
            "https://wikipedia.org@evil.example/",
            &domains
        ));
        // No host at all.
        assert!(!url_host_matches_domain_list(
            "mailto:foo@wikipedia.org",
            &domains
        ));
        assert!(!url_host_matches_domain_list("/relative/path", &domains));
    }
}
