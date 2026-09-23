//! Revision-pinned client for a wiki's REST/Action API.
//!
//! Everything here is deliberately explicit about **revision pinning**. The
//! comparison is only meaningful if the wikitext and the Parsoid HTML come from
//! the same revision, so the flow is always:
//!
//! 1. resolve the latest `revid` for a title (or take one the caller supplied),
//! 2. fetch both artifacts *at that revid*.
//!
//! Verified endpoints (checked live against `en.wikipedia.org`):
//!
//! | Purpose | Request |
//! |---|---|
//! | latest revid | `api.php?action=query&prop=revisions&titles=…&rvprop=ids` |
//! | wikitext at a revision | `rest.php/v1/revision/{revid}` → `.source` |
//! | Parsoid HTML, latest | `rest.php/v1/page/{title}/html` |
//! | Parsoid HTML, pinned | `api/rest_v1/page/html/{title}/{revision}` |
//! | site config | `api.php?action=query&meta=siteinfo&siprop=…` |
//!
//! Note the HTML endpoints are *not* symmetric: `rest.php/v1/page/{title}/{rev}`
//! 404s, whereas `rest.php/v1/revision/{rev}` is fine. The pinned HTML endpoint
//! is the older `api/rest_v1` form.
//!
//! Wikimedia rate-limits aggressively — a handful of rapid requests already
//! returns `You are making too many requests to the API` — so the client sends a
//! descriptive `User-Agent` and the harness is expected to cache every response.

use serde::Deserialize;

use crate::error::{CompareError, Result};

/// Default `User-Agent`. Wikimedia rejects requests without a descriptive one.
pub const DEFAULT_USER_AGENT: &str =
    "rustoid-compare/0.1 (https://github.com/magnusmanske/rustoid; parser parity testing)";

/// A wiki host, e.g. `en.wikipedia.org`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wiki {
    pub host: String,
}

impl Wiki {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn api_url(&self) -> String {
        format!("https://{}/w/api.php", self.host)
    }

    pub fn rest_url(&self) -> String {
        format!("https://{}/w/rest.php/v1", self.host)
    }

    /// The older REST base, which is where the revision-pinned HTML endpoint
    /// lives.
    pub fn rest_v1_url(&self) -> String {
        format!("https://{}/api/rest_v1", self.host)
    }
}

/// Client for one wiki.
///
/// Cheap to clone: `reqwest::Client` is an `Arc` internally, so clones share the
/// connection pool (which matters for the rate limiter).
#[derive(Clone)]
pub struct WikiClient {
    wiki: Wiki,
    http: reqwest::Client,
    /// Paces requests, shared across every clone.
    limiter: std::sync::Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
}

/// Minimum spacing between two requests to a wiki.
///
/// Wikimedia's rate limiter is real and immediate — a handful of rapid requests
/// already returns `You are making too many requests to the API` — and template
/// expansion is the case that trips it: one page can transclude hundreds of
/// templates, each a separate fetch, with no user-visible pacing. The harness's
/// `--delay-ms` only spaces *pages*, so it cannot cover this.
///
/// Zero when `RUSTOID_NO_THROTTLE` is set, which is what a test against a local
/// file server wants.
const MIN_REQUEST_SPACING: std::time::Duration = std::time::Duration::from_millis(120);

impl WikiClient {
    pub fn new(wiki: Wiki) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            // A whole-request cap. Without it, and without a connect cap, a
            // request can sit on a pooled keep-alive connection that the wiki's
            // load balancer has silently dropped: the socket stays ESTABLISHED,
            // nothing is ever sent or received, and the process waits forever.
            // That wedged an entire corpus run at one page.
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(30))
            // A pooled connection is only worth reusing for a short while. The
            // failure above is far likelier on a connection that has been idle,
            // and reconnecting is cheap next to hanging.
            .pool_idle_timeout(std::time::Duration::from_secs(15))
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|e| CompareError::Http {
                url: wiki.api_url(),
                message: format!("client build: {e}"),
                status: 0,
            })?;
        Ok(Self {
            wiki,
            http,
            limiter: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    pub fn wiki(&self) -> &Wiki {
        &self.wiki
    }

    /// Sleep until at least [`MIN_REQUEST_SPACING`] has passed since the last
    /// request.
    ///
    /// The spacing is enforced *between* requests rather than per request, so a
    /// burst of template fetches is spread out instead of queued behind a fixed
    /// delay each. The mutex is held across the sleep deliberately: two concurrent
    /// callers must serialise against the same clock, or they would both see the
    /// same idle moment and fire together.
    async fn throttle(&self) {
        if std::env::var_os("RUSTOID_NO_THROTTLE").is_some() {
            return;
        }
        let mut last = self.limiter.lock().await;
        let now = std::time::Instant::now();
        if let Some(previous) = *last
            && let Some(wait) = MIN_REQUEST_SPACING.checked_sub(now.duration_since(previous))
            && !wait.is_zero()
        {
            tokio::time::sleep(wait).await;
        }
        *last = Some(std::time::Instant::now());
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        // One retry, for transport failures only. A dropped connection is common
        // enough on a long run that losing a page to it would make corpus scores
        // noisy, but a retry is safe here because every request is a plain GET of
        // revision-pinned content.
        //
        // A request that got a status is *not* retried: the server answered, and
        // asking again would only waste a request on a definitive 404.
        match self.get_text_once(url).await {
            Ok(body) => Ok(body),
            Err(CompareError::Http { status: 0, .. }) => self.get_text_once(url).await,
            Err(e) => Err(e),
        }
    }

    async fn get_text_once(&self, url: &str) -> Result<String> {
        self.throttle().await;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| CompareError::Http {
                url: url.to_string(),
                message: e.to_string(),
                status: 0,
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| CompareError::Http {
            url: url.to_string(),
            message: e.to_string(),
            status: 0,
        })?;
        if !status.is_success() {
            return Err(CompareError::Http {
                url: url.to_string(),
                message: format!("HTTP {status}: {}", truncate(&body, 200)),
                status: status.as_u16(),
            });
        }
        Ok(body)
    }

    /// Resolve the latest revision id for `title`.
    ///
    /// Returns `None` when the page does not exist (the API reports a negative
    /// pageid), which the caller should distinguish from a transport failure.
    pub async fn latest_revid(&self, title: &str) -> Result<Option<u64>> {
        let url = format!(
            "{}?action=query&prop=revisions&rvprop=ids&format=json&formatversion=2&titles={}",
            self.wiki.api_url(),
            urlencode(title)
        );
        let body = self.get_text(&url).await?;
        let parsed: QueryRevisions =
            serde_json::from_str(&body).map_err(|e| CompareError::Response {
                url: url.clone(),
                message: format!("query parse: {e}"),
            })?;
        let Some(page) = parsed.query.pages.into_iter().next() else {
            return Ok(None);
        };
        if page.missing {
            return Ok(None);
        }
        Ok(page.revisions.and_then(|r| r.first().map(|x| x.revid)))
    }

    /// Fetch the wikitext of `revid`.
    pub async fn wikitext_at(&self, revid: u64) -> Result<String> {
        let url = format!("{}/revision/{}", self.wiki.rest_url(), revid);
        let body = self.get_text(&url).await?;
        let parsed: RevisionResponse =
            serde_json::from_str(&body).map_err(|e| CompareError::Response {
                url: url.clone(),
                message: format!("revision parse: {e}"),
            })?;
        let Some(source) = parsed.source else {
            return Err(CompareError::Response {
                url,
                message: "revision response had no `source`".to_string(),
            });
        };
        Ok(source)
    }

    /// Fetch the wiki's own Parsoid HTML for `title` at `revid`.
    ///
    /// Uses the pinned endpoint so the HTML cannot drift away from the wikitext.
    pub async fn parsoid_html_at(&self, title: &str, revid: u64) -> Result<String> {
        let url = format!(
            "{}/page/html/{}/{}",
            self.wiki.rest_v1_url(),
            urlencode(title),
            revid
        );
        self.get_text(&url).await
    }

    /// Render `wikitext` with the wiki's own Parsoid, as if it were the source of
    /// `title`.
    ///
    /// This is the oracle for input the caller *chose*, which the rest of the
    /// harness cannot provide: every other comparison is against a page the wiki
    /// already has, so varying the input means finding a cached page that
    /// contains the construct — and the construct then arrives buried in
    /// hundreds of kilobytes of unrelated markup, as a moving target. A
    /// reduction needs to hold everything else still and change one thing.
    ///
    /// The title is not decoration: it is what Parsoid resolves `{{PAGENAME}}`,
    /// the subject namespace and the page-scoped magic words against, so a
    /// reduction should name a title in the namespace it means. The templates the
    /// wikitext transcludes are resolved and fetched as usual.
    pub async fn parsoid_html_for_wikitext(&self, title: &str, wikitext: &str) -> Result<String> {
        let url = format!(
            "{}/transform/wikitext/to/html/{}",
            self.wiki.rest_url(),
            urlencode(title)
        );
        let body = format!("wikitext={}", urlencode(wikitext));
        self.post_form(&url, &body).await
    }

    /// POST an already form-encoded body, returning the response text.
    ///
    /// The transform endpoint is the only POST here, and the body is encoded by
    /// the same `urlencode` the GETs use rather than by a form serializer: one
    /// encoder means the title in the path and the wikitext in the body cannot
    /// disagree about how a space is spelled.
    async fn post_form(&self, url: &str, body: &str) -> Result<String> {
        // Retried like the GETs, and for the same reason: a dropped connection
        // would otherwise lose a reduction. A response with a status is not
        // retried, because the server answered.
        match self.post_form_once(url, body).await {
            Ok(text) => Ok(text),
            Err(CompareError::Http { status: 0, .. }) => self.post_form_once(url, body).await,
            Err(e) => Err(e),
        }
    }

    async fn post_form_once(&self, url: &str, body: &str) -> Result<String> {
        self.throttle().await;
        let resp = self
            .http
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| CompareError::Http {
                url: url.to_string(),
                message: e.to_string(),
                status: 0,
            })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| CompareError::Http {
            url: url.to_string(),
            message: e.to_string(),
            status: 0,
        })?;
        if !status.is_success() {
            return Err(CompareError::Http {
                url: url.to_string(),
                message: format!("HTTP {status}: {}", truncate(&text, 200)),
                status: status.as_u16(),
            });
        }
        Ok(text)
    }

    /// Fetch a Wikidata entity as JSON, or `None` when the id does not exist.
    ///
    /// `Special:EntityData/<id>.json` is the canonical way to read one entity:
    /// a single request, and the body is the serialisation `mw.wikibase` itself
    /// consumes. Going through the page's wikitext instead would mean parsing
    /// JSON out of a wiki page, and `wbgetentities` would return a batch
    /// envelope that has to be unwrapped.
    ///
    /// A missing entity answers 404, which is a real answer ("no such id") and
    /// not an error: `mw.wikibase.entityExists` has to report false.
    pub async fn entity_json(&self, id: &str) -> Result<Option<String>> {
        let url = format!(
            "https://{}/wiki/Special:EntityData/{}.json",
            self.wiki.host,
            urlencode(id)
        );
        match self.get_text(&url).await {
            Ok(body) => Ok(Some(body)),
            Err(CompareError::Http { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The entity id that has `title` as its sitelink on `site`, if any.
    ///
    /// This is the query `getEntityIdForCurrentPage` and `getEntityIdForTitle`
    /// answer from, and it is a *search*: the page title alone determines the
    /// entity, so a module that never names an id still reaches entity data.
    ///
    /// `wbgetentities` with `sites`/`titles` is the direct form; the alternative
    /// is a sitelink search that would return a page of candidates to filter.
    /// Returns `None` for a title with no entity, which is normal: most articles
    /// have one, but a red link or a project page does not.
    pub async fn entity_id_for_title(&self, site: &str, title: &str) -> Result<Option<String>> {
        let url = format!(
            "{}?action=wbgetentities&sites={}&titles={}&props=info&format=json&formatversion=2",
            self.wiki.api_url(),
            urlencode(site),
            urlencode(title)
        );
        let body = self.get_text(&url).await?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| CompareError::Response {
                url: url.clone(),
                message: e.to_string(),
            })?;
        // A missing sitelink comes back as `{"entities":{"-1":{...}}}` rather
        // than as an error, so the id is read off a real entity only.
        let Some(entity) = parsed
            .get("entities")
            .and_then(|e| e.as_object())
            .and_then(|o| o.values().next())
        else {
            return Ok(None);
        };
        if entity.get("missing").is_some() {
            return Ok(None);
        }
        Ok(entity
            .get("id")
            .and_then(|i| i.as_str())
            .map(str::to_string))
    }

    /// Existence metadata for up to 50 titles, with no wikitext.
    ///
    /// `prop=info` answers "does this exist, and is it a redirect?" in one
    /// request, which is what link resolution needs. Fetching content instead
    /// would be two requests per link.
    pub async fn page_info_json(&self, titles: &[String]) -> Result<String> {
        let joined = titles
            .iter()
            .map(|t| urlencode(t))
            .collect::<Vec<_>>()
            .join("%7C");
        let url = format!(
            "{}?action=query&prop=info&format=json&formatversion=2&titles={}",
            self.wiki.api_url(),
            joined
        );
        self.get_text(&url).await
    }

    /// Page protection levels for up to 50 titles, with no wikitext.
    ///
    /// `inprop=protection` is what `mw.title`'s `protectionLevels` reads. It is
    /// a separate request from [`WikiClient::page_info_json`] rather than a flag
    /// on it, because only `mw.title` needs it and link resolution asks about
    /// hundreds of titles per page.
    pub async fn protection_json(&self, titles: &[String]) -> Result<String> {
        let joined = titles
            .iter()
            .map(|t| urlencode(t))
            .collect::<Vec<_>>()
            .join("%7C");
        let url = format!(
            "{}?action=query&prop=info&inprop=protection&format=json&formatversion=2&titles={}",
            self.wiki.api_url(),
            joined
        );
        self.get_text(&url).await
    }

    /// Fetch raw `siteinfo` JSON (namespaces, magic words, function hooks,
    /// extension tags, interwiki map, general).
    pub async fn siteinfo(&self) -> Result<String> {
        let url = format!(
            "{}?action=query&meta=siteinfo&siprop=general%7Cnamespaces%7Cnamespacealiases%7Cmagicwords%7Cfunctionhooks%7Cextensiontags%7Cinterwikimap%7Cstatistics&format=json&formatversion=2",
            self.wiki.api_url()
        );
        self.get_text(&url).await
    }
}

/// Percent-encode a title for use in a query string or path segment.
///
/// `url::form_urlencoded` is not used because it encodes spaces as `+`, which is
/// wrong in a path segment; `%20` is correct in both positions.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Char-boundary safe: find the largest boundary <= n.
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

// ---- wire types ----

#[derive(Debug, Deserialize)]
struct QueryRevisions {
    query: QueryPages,
}

#[derive(Debug, Deserialize)]
struct QueryPages {
    #[serde(default)]
    pages: Vec<PageEntry>,
}

#[derive(Debug, Deserialize)]
struct PageEntry {
    #[serde(default)]
    missing: bool,
    #[serde(default)]
    revisions: Option<Vec<RevisionEntry>>,
}

#[derive(Debug, Deserialize)]
struct RevisionEntry {
    revid: u64,
}

#[derive(Debug, Deserialize)]
struct RevisionResponse {
    #[serde(default)]
    source: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two calls in immediate succession must be spaced apart.
    ///
    /// The point is not the exact duration but that the second call *waits*: an
    /// unpaced burst is what earns `You are making too many requests`, and a
    /// template-heavy page fires these with no other pacing at all.
    #[tokio::test]
    async fn requests_are_spaced_apart() {
        let client = WikiClient::new(Wiki::new("example.invalid")).unwrap();
        let start = std::time::Instant::now();
        client.throttle().await;
        let after_first = start.elapsed();
        client.throttle().await;
        let after_second = start.elapsed();
        assert!(
            after_first < MIN_REQUEST_SPACING / 2,
            "the first request should not wait: {after_first:?}"
        );
        assert!(
            after_second >= MIN_REQUEST_SPACING,
            "the second must wait for the spacing: {after_second:?}"
        );
    }

    /// The spacing must be *between* requests, not a fixed delay on each.
    ///
    /// With per-request delay, N requests cost N × spacing; with spacing-between,
    /// they cost (N-1) × spacing plus the call overhead. The latter is what keeps a
    /// hundreds-of-templates page tractable.
    #[tokio::test]
    async fn an_idle_gap_is_not_charged_twice() {
        let client = WikiClient::new(Wiki::new("example.invalid")).unwrap();
        client.throttle().await;
        tokio::time::sleep(MIN_REQUEST_SPACING * 2).await;
        let start = std::time::Instant::now();
        client.throttle().await;
        assert!(
            start.elapsed() < MIN_REQUEST_SPACING / 2,
            "an already-idle client must not wait again: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn wiki_urls_are_built_from_the_host() {
        let w = Wiki::new("en.wikipedia.org/");
        assert_eq!(w.host, "en.wikipedia.org");
        assert_eq!(w.api_url(), "https://en.wikipedia.org/w/api.php");
        assert_eq!(w.rest_url(), "https://en.wikipedia.org/w/rest.php/v1");
        assert_eq!(w.rest_v1_url(), "https://en.wikipedia.org/api/rest_v1");
    }

    #[test]
    fn title_encoding_uses_percent_twenty_for_spaces() {
        assert_eq!(urlencode("Main Page"), "Main%20Page");
        assert_eq!(urlencode("Template:Foo/bar"), "Template%3AFoo%2Fbar");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        // Unreserved characters pass through.
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn encoding_is_byte_wise_and_round_trips_utf8() {
        // Non-ASCII must be percent-encoded per UTF-8 byte.
        assert_eq!(urlencode("é"), "%C3%A9");
        assert_eq!(urlencode("日本"), "%E6%97%A5%E6%9C%AC");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        // Must not panic when the cut lands inside a multi-byte char.
        let s = "日本語テキスト";
        let t = truncate(s, 4);
        assert!(t.ends_with('…'));
        assert!(s.starts_with(t.trim_end_matches('…')));
    }

    #[test]
    fn latest_revid_parses_the_query_shape() {
        // Shape captured from the live API (formatversion=2).
        let body = r#"{"batchcomplete":true,"query":{"pages":[{"pageid":1,"ns":0,"title":"UFC BJJ","revisions":[{"revid":1300000000,"parentid":1299999999}]}]}}"#;
        let parsed: QueryRevisions = serde_json::from_str(body).unwrap();
        let page = &parsed.query.pages[0];
        assert!(!page.missing);
        assert_eq!(page.revisions.as_ref().unwrap()[0].revid, 1300000000);
    }

    #[test]
    fn missing_page_is_recognised() {
        let body = r#"{"query":{"pages":[{"ns":0,"title":"Nope","missing":true}]}}"#;
        let parsed: QueryRevisions = serde_json::from_str(body).unwrap();
        assert!(parsed.query.pages[0].missing);
    }

    #[test]
    fn revision_response_parses_source() {
        let body = r#"{"id":1300000000,"source":"{{Short description|x}}"}"#;
        let parsed: RevisionResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.source.unwrap(), "{{Short description|x}}");
    }
}
