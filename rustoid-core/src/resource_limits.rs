//! wt2html resource limits — faithful port of the limit half of PHP's
//! `src/Config/Env.php` (`wt2htmlUsage`, `bumpWt2HtmlResourceUse`,
//! `compareWt2HtmlLimit`) plus the `wt2htmlLimits` defaults from
//! `SiteConfig`.
//!
//! Parsoid refuses to spend unbounded work on a single parse. Each resource is
//! a named counter with an optional ceiling; the parser bumps the counter as it
//! consumes the resource and reacts when a bump crosses the ceiling.

use std::collections::HashMap;

use crate::traits::SiteConfig;

/// Per-parse wt2html resource ceilings. A resource absent from the map is
/// unlimited. Mirrors PHP `SiteConfig::$wt2htmlLimits`.
#[derive(Debug, Clone)]
pub struct Wt2HtmlLimits(HashMap<String, i64>);

impl Default for Wt2HtmlLimits {
    fn default() -> Self {
        // Mirrors `SiteConfig::$wt2htmlLimits` (the same defaults are inherited
        // by every site config, including the parser-test mock).
        Self(HashMap::from([
            // We won't handle pages beyond this size.
            // `ParserOptions::maxIncludeSize`.
            ("wikitextSize".to_string(), 2048 * 1024),
            // Max list items per page.
            ("listItem".to_string(), 30000),
            // Max table cells per page.
            ("tableCell".to_string(), 30000),
            // Max transclusions per page.
            ("transclusion".to_string(), 10000),
            // Max images per page.
            ("image".to_string(), 5000),
            // Max top-level token size.
            ("token".to_string(), 1000000),
        ]))
    }
}

impl Wt2HtmlLimits {
    /// The ceiling for `resource`, or `None` when unlimited.
    pub fn get(&self, resource: &str) -> Option<i64> {
        self.0.get(resource).copied()
    }

    /// Override (or add) a ceiling. Mirrors PHP `SiteConfig::setWt2htmlLimit`.
    pub fn set(&mut self, resource: impl Into<String>, limit: i64) {
        self.0.insert(resource.into(), limit);
    }

    /// Whether using `n` units of `resource` is still within the limit.
    /// Mirrors `Env::compareWt2HtmlLimit`: `!( isset($limits[$r]) && $n > $limits[$r] )`.
    pub fn compare(&self, resource: &str, n: i64) -> bool {
        !self.get(resource).is_some_and(|limit| n > limit)
    }
}

/// The result of a resource-use bump. Mirrors `Env::bumpWt2HtmlResourceUse`'s
/// `?bool` return: `null` in PHP is [`Bump::AlreadyOver`], `false` is
/// [`Bump::Crossed`] (this bump pushed the count over the ceiling), and `true`
/// is [`Bump::Within`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bump {
    /// Still within the limit.
    Within,
    /// This bump pushed the count over the ceiling (PHP `false`). Callers use
    /// this to report the limit once.
    Crossed,
    /// The count was already over the ceiling before this bump (PHP `null`), so
    /// the caller stays quiet to avoid logging every subsequent token.
    AlreadyOver,
}

/// Per-parse resource usage counters. Mirrors `Env::$wt2htmlUsage` and its two
/// methods.
#[derive(Debug, Default)]
pub struct ResourceUsage {
    usage: HashMap<String, i64>,
    limits: Wt2HtmlLimits,
}

impl ResourceUsage {
    /// Create counters bound to a site config's limits.
    pub fn new(config: &dyn SiteConfig) -> Self {
        Self {
            limits: config.wt2html_limits(),
            usage: HashMap::new(),
        }
    }

    /// Create counters with explicit limits.
    pub fn with_limits(limits: Wt2HtmlLimits) -> Self {
        Self {
            limits,
            usage: HashMap::new(),
        }
    }

    /// Whether using `n` units of `resource` is still within the limit.
    /// Mirrors `Env::compareWt2HtmlLimit`.
    pub fn compare(&self, resource: &str, n: i64) -> bool {
        self.limits.compare(resource, n)
    }

    /// Record `count` more units of `resource`. Mirrors
    /// `Env::bumpWt2HtmlResourceUse` — including its deliberate off-by-one: the
    /// *look-ahead* check uses the pre-bump count, so an unlimited resource
    /// returns [`Bump::Within`] and a limited one reports [`Bump::Crossed`] on
    /// the bump that first exceeds the ceiling (`n > limit`, not `n >= limit`).
    pub fn bump(&mut self, resource: &str, count: i64) -> Bump {
        let n = self.usage.get(resource).copied().unwrap_or(0);
        if !self.limits.compare(resource, n) {
            return Bump::AlreadyOver;
        }
        let n = n + count;
        self.usage.insert(resource.to_string(), n);
        if self.limits.compare(resource, n) {
            Bump::Within
        } else {
            Bump::Crossed
        }
    }

    /// [`Self::bump`] with a count of 1.
    pub fn bump_one(&mut self, resource: &str) -> Bump {
        self.bump(resource, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limited(limit: i64) -> ResourceUsage {
        let mut limits = Wt2HtmlLimits::default();
        limits.set("image", limit);
        ResourceUsage::with_limits(limits)
    }

    #[test]
    fn bump_matches_php_off_by_one() {
        // With a ceiling of 2 the sequence is true, true, false, null, null …
        // (verified against `Env::bumpWt2HtmlResourceUse`).
        let mut u = limited(2);
        assert_eq!(u.bump_one("image"), Bump::Within);
        assert_eq!(u.bump_one("image"), Bump::Within);
        assert_eq!(u.bump_one("image"), Bump::Crossed);
        assert_eq!(u.bump_one("image"), Bump::AlreadyOver);
        assert_eq!(u.bump_one("image"), Bump::AlreadyOver);
    }

    #[test]
    fn unlimited_resource_never_crosses() {
        let mut u = ResourceUsage::with_limits(Wt2HtmlLimits(HashMap::new()));
        for _ in 0..10 {
            assert_eq!(u.bump_one("image"), Bump::Within);
        }
    }

    #[test]
    fn compare_uses_strict_greater_than() {
        let u = limited(2);
        assert!(u.compare("image", 0));
        assert!(u.compare("image", 2));
        assert!(!u.compare("image", 3));
    }

    #[test]
    fn bump_accumulates_by_count() {
        let mut u = limited(10);
        // `wikitextSize` is bumped with the page size, not by one.
        assert_eq!(u.bump("image", 4), Bump::Within);
        assert_eq!(u.bump("image", 4), Bump::Within);
        assert_eq!(u.bump("image", 4), Bump::Crossed);
    }
}
