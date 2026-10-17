//! `SecureLinkFixer`: upgrade `http://` (and protocol-relative) links to
//! `https://` when the host is on Mozilla's HSTS preload list.
//!
//! Port of MediaWiki's `SecureLinkFixer` extension: `Hooks.php` hooks
//! `LinkerMakeExternalLink`, and `HSTSPreloadLookup.php` answers from the
//! generated `domains.php`. A live wiki applies the same hook while Parsoid
//! builds a link, so the served `href` differs from the written one and the
//! written form is kept in `data-mw-original-href` (`Env::getExternalLinkAttribs`).
//!
//! The table is Mozilla's, shipped as two sorted text files: domains whose
//! subdomains inherit the entry, and the (few) entries marked "no subdomains".

use std::sync::OnceLock;

const PRELOADED: &str = include_str!("../data/hsts_preload.txt");
const NO_SUBDOMAINS: &str = include_str!("../data/hsts_preload_no_subdomains.txt");

fn domains(data: &'static str) -> Vec<&'static str> {
    data.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn preloaded() -> &'static [&'static str] {
    static TABLE: OnceLock<Vec<&'static str>> = OnceLock::new();
    TABLE.get_or_init(|| domains(PRELOADED))
}

fn no_subdomains() -> &'static [&'static str] {
    static TABLE: OnceLock<Vec<&'static str>> = OnceLock::new();
    TABLE.get_or_init(|| domains(NO_SUBDOMAINS))
}

fn contains(table: &[&str], domain: &str) -> bool {
    table.binary_search(&domain).is_ok()
}

/// `HSTSPreloadLookup::isPreloaded`.
///
/// An exact match is preloaded whatever the entry's value; the value only
/// decides whether a *parent* match applies (a `0` entry stops the walk).
pub fn is_preloaded(host: &str) -> bool {
    if contains(preloaded(), host) || contains(no_subdomains(), host) {
        return true;
    }
    let mut rest = host;
    while let Some(pos) = rest.find('.') {
        rest = &rest[pos + 1..];
        if contains(preloaded(), rest) {
            return true;
        }
        if contains(no_subdomains(), rest) {
            return false;
        }
    }
    false
}

/// `Hooks::onLinkerMakeExternalLink`: the rewritten URL, or `None` when the
/// link is left as written.
pub fn rewrite(href: &str) -> Option<String> {
    if href.starts_with("https://") {
        return None;
    }
    let rest = strip_http_scheme(href).or_else(|| href.strip_prefix("//"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_of(authority);
    if host.is_empty() || !is_preloaded(host) {
        return None;
    }
    Some(format!("https://{rest}"))
}

/// The part after `http://`, matched case-insensitively like `UrlUtils::parse`.
fn strip_http_scheme(href: &str) -> Option<&str> {
    let (scheme, rest) = href.split_once("://")?;
    scheme.eq_ignore_ascii_case("http").then_some(rest)
}

/// `parse_url`'s `host`: drop userinfo and the port (an IPv6 literal keeps its
/// brackets and may carry a port after them).
fn host_of(authority: &str) -> &str {
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host_port.starts_with('[') {
        host_port
            .find(']')
            .map_or(host_port, |end| &host_port[..=end])
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preloaded_host_is_upgraded() {
        // The domain of the URL on `Canada`'s reference section.
        assert!(is_preloaded("laws-lois.justice.gc.ca"));
        assert_eq!(
            rewrite("http://laws-lois.justice.gc.ca/eng/acts/n-16.7/page-1.html"),
            Some("https://laws-lois.justice.gc.ca/eng/acts/n-16.7/page-1.html".to_string())
        );
        assert_eq!(
            rewrite("//laws-lois.justice.gc.ca/x"),
            Some("https://laws-lois.justice.gc.ca/x".to_string())
        );
    }

    #[test]
    fn only_http_and_protocol_relative_are_rewritten() {
        assert_eq!(rewrite("https://laws-lois.justice.gc.ca/x"), None);
        assert_eq!(rewrite("ftp://laws-lois.justice.gc.ca/x"), None);
        assert_eq!(rewrite("http://example.invalid/x"), None);
    }

    #[test]
    fn a_no_subdomains_entry_matches_itself_but_not_children() {
        // `002.ro` is `=> 0` in the generated table.
        assert!(is_preloaded("002.ro"));
        assert!(!is_preloaded("www.002.ro"));
    }

    #[test]
    fn userinfo_and_ports_are_not_part_of_the_host() {
        assert_eq!(
            rewrite("http://user@laws-lois.justice.gc.ca:8080/x"),
            Some("https://user@laws-lois.justice.gc.ca:8080/x".to_string())
        );
        assert_eq!(
            rewrite("http://laws-lois.justice.gc.ca:8080/x"),
            Some("https://laws-lois.justice.gc.ca:8080/x".to_string())
        );
    }
}
