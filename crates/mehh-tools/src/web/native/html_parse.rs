//! Clean-room SERP HTML extractors for DuckDuckGo HTML and Bing HTML using `html_markdown::scan_html`.

use std::borrow::Cow;

use base64::Engine as _;
use reqwest::Url;

use crate::web::html_markdown::{
    contains_ascii_ci, decode_entities, find_attr_decoded, scan_html, HtmlSink,
};
use crate::web::native::rank::{RawHit, SourceId};

/// Checks common CAPTCHA or bot-wall signatures in HTML search responses.
pub(crate) fn looks_like_captcha(html: &str) -> bool {
    let bytes = html.as_bytes();
    contains_ascii_ci(bytes, b"anomaly-modal")
        || contains_ascii_ci(bytes, b"g-recaptcha")
        || contains_ascii_ci(bytes, b"cf-challenge")
        || contains_ascii_ci(bytes, b"challenge-platform")
        || contains_ascii_ci(bytes, b"unusual traffic")
        || contains_ascii_ci(bytes, b"please verify you are a human")
        || contains_ascii_ci(bytes, b"verify you are human")
        || contains_ascii_ci(bytes, b"captcha-form")
}

fn host_matches_domain(host: &str, domain: &str) -> bool {
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    let suffix_len = domain.len() + 1;
    if host.len() < suffix_len {
        return false;
    }
    let split_at = host.len() - domain.len();
    host.as_bytes()[split_at - 1] == b'.' && host[split_at..].eq_ignore_ascii_case(domain)
}

/// Unwraps DuckDuckGo `/l/?uddg=...` redirect URLs and skips DDG ad links.
pub(crate) fn unwrap_ddg_url(raw_href: &str) -> Option<String> {
    let trimmed = raw_href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let trimmed_bytes = trimmed.as_bytes();
    if contains_ascii_ci(trimmed_bytes, b"duckduckgo.com/y.js")
        || contains_ascii_ci(trimmed_bytes, b"ad_provider=")
    {
        return None;
    }

    let normalized: Cow<'_, str> = if trimmed.starts_with("//") {
        Cow::Owned(format!("https:{trimmed}"))
    } else if trimmed.starts_with('/') {
        Cow::Owned(format!("https://duckduckgo.com{trimmed}"))
    } else {
        Cow::Borrowed(trimmed)
    };

    let url = Url::parse(&normalized).ok()?;
    let host = url.host_str().unwrap_or("");
    if host_matches_domain(host, "duckduckgo.com") && url.path().starts_with("/l/") {
        for (k, v) in url.query_pairs() {
            if k == "uddg" {
                let target = Url::parse(v.trim()).ok()?;
                return matches!(target.scheme(), "http" | "https").then(|| target.into());
            }
        }
        return None;
    }

    matches!(url.scheme(), "http" | "https").then(|| url.into())
}

/// Unwraps Bing `/ck/a?...&u=a1<base64url>` redirect URLs and drops Bing ad/internal links.
pub(crate) fn unwrap_bing_url(raw_href: &str) -> Option<String> {
    let trimmed = raw_href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let normalized: Cow<'_, str> = if trimmed.starts_with("//") {
        Cow::Owned(format!("https:{trimmed}"))
    } else if trimmed.starts_with('/') {
        Cow::Owned(format!("https://www.bing.com{trimmed}"))
    } else {
        Cow::Borrowed(trimmed)
    };

    let url = Url::parse(&normalized).ok()?;
    let host = url.host_str().unwrap_or("");
    if host_matches_domain(host, "bing.com") {
        if url.path().starts_with("/aclick") {
            return None;
        }
        if url.path().starts_with("/ck/a") {
            for (k, v) in url.query_pairs() {
                if k == "u" {
                    let encoded = v.strip_prefix("a1")?.trim_end_matches('=');
                    let decoded_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(encoded)
                        .ok()?;
                    let decoded_str = String::from_utf8(decoded_bytes).ok()?;
                    let target = Url::parse(decoded_str.trim()).ok()?;
                    return matches!(target.scheme(), "http" | "https").then(|| target.into());
                }
            }
            return None;
        }
    }

    matches!(url.scheme(), "http" | "https").then(|| url.into())
}

fn has_class_token(class_attr: &str, token: &str) -> bool {
    class_attr
        .split(|c: char| c.is_ascii_whitespace())
        .any(|c| c.eq_ignore_ascii_case(token))
}

fn clean_html_text(raw: &str) -> Option<String> {
    let decoded = decode_entities(raw);
    let mut out = String::with_capacity(decoded.len());
    for word in decoded.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    (!out.is_empty()).then_some(out)
}

struct DdgParser {
    div_depth: usize,
    in_result: bool,
    in_title: bool,
    title_anchor_depth: usize,
    in_snippet: bool,
    snippet_tag_depth: usize,
    current_href: Option<String>,
    current_title: String,
    current_snippet: String,
    hits: Vec<RawHit>,
}

impl DdgParser {
    fn new() -> Self {
        Self {
            div_depth: 0,
            in_result: false,
            in_title: false,
            title_anchor_depth: 0,
            in_snippet: false,
            snippet_tag_depth: 0,
            current_href: None,
            current_title: String::new(),
            current_snippet: String::new(),
            hits: Vec::new(),
        }
    }

    fn flush_current(&mut self) {
        if let Some(href) = self.current_href.take() {
            if let Some(url) = unwrap_ddg_url(&href) {
                let rank = self.hits.len() + 1;
                self.hits.push(RawHit {
                    source: SourceId::DuckDuckGo,
                    rank,
                    url,
                    title: clean_html_text(&self.current_title),
                    snippet: clean_html_text(&self.current_snippet),
                    published: None,
                });
            }
        }
        self.current_title.clear();
        self.current_snippet.clear();
        self.in_title = false;
        self.in_snippet = false;
        self.title_anchor_depth = 0;
        self.snippet_tag_depth = 0;
    }
}

impl HtmlSink for DdgParser {
    fn on_start_tag(&mut self, name: &str, raw_attrs: &str, self_closing: bool) {
        if self_closing {
            return;
        }
        if name == "div" {
            if self.in_result {
                self.div_depth += 1;
            } else if let Some(cls) = find_attr_decoded(raw_attrs, "class") {
                if (has_class_token(&cls, "result") || has_class_token(&cls, "web-result"))
                    && !has_class_token(&cls, "result--ad")
                    && !has_class_token(&cls, "results")
                {
                    self.in_result = true;
                    self.div_depth = 1;
                    self.current_href = None;
                    self.current_title.clear();
                    self.current_snippet.clear();
                }
            }
        }

        if !self.in_result {
            return;
        }

        if !raw_attrs.is_empty() && (self.current_href.is_none() || self.current_snippet.is_empty()) {
            if let Some(cls) = find_attr_decoded(raw_attrs, "class") {
                if name == "a" && has_class_token(&cls, "result__a") && self.current_href.is_none() {
                    self.current_href =
                        find_attr_decoded(raw_attrs, "href").map(|h| h.into_owned());
                    self.in_title = true;
                    self.title_anchor_depth = 1;
                    return;
                }
                if has_class_token(&cls, "result__snippet") && self.current_snippet.is_empty() {
                    self.in_snippet = true;
                    self.snippet_tag_depth = 1;
                    return;
                }
            }
        }

        if self.in_title && name == "a" {
            self.title_anchor_depth += 1;
        }
        if self.in_snippet {
            if name == "br" {
                self.current_snippet.push(' ');
            }
            if !is_html_void_tag(name) {
                self.snippet_tag_depth += 1;
            }
        }
    }

    fn on_end_tag(&mut self, name: &str) {
        if !self.in_result {
            return;
        }
        if self.in_title && name == "a" {
            self.title_anchor_depth = self.title_anchor_depth.saturating_sub(1);
            if self.title_anchor_depth == 0 {
                self.in_title = false;
            }
        }
        if self.in_snippet {
            self.snippet_tag_depth = self.snippet_tag_depth.saturating_sub(1);
            if self.snippet_tag_depth == 0 {
                self.in_snippet = false;
            }
        }
        if name == "div" {
            self.div_depth = self.div_depth.saturating_sub(1);
            if self.div_depth == 0 {
                self.in_result = false;
                self.flush_current();
            }
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.in_title {
            self.current_title.push_str(text);
        } else if self.in_snippet {
            self.current_snippet.push_str(text);
        }
    }
}

fn is_html_void_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Extracts organic web search results from DuckDuckGo HTML SERP responses.
pub(crate) fn parse_ddg_html(html: &str) -> Vec<RawHit> {
    let mut parser = DdgParser::new();
    scan_html(html, &mut parser);
    if parser.in_result {
        parser.flush_current();
    }
    parser.hits
}

struct BingParser {
    li_depth: usize,
    in_result: bool,
    in_h2: bool,
    in_title: bool,
    in_snippet: bool,
    snippet_done: bool,
    current_href: Option<String>,
    current_title: String,
    current_snippet: String,
    hits: Vec<RawHit>,
}

impl BingParser {
    fn new() -> Self {
        Self {
            li_depth: 0,
            in_result: false,
            in_h2: false,
            in_title: false,
            in_snippet: false,
            snippet_done: false,
            current_href: None,
            current_title: String::new(),
            current_snippet: String::new(),
            hits: Vec::new(),
        }
    }

    fn flush_current(&mut self) {
        if let Some(href) = self.current_href.take() {
            if let Some(url) = unwrap_bing_url(&href) {
                let rank = self.hits.len() + 1;
                self.hits.push(RawHit {
                    source: SourceId::Bing,
                    rank,
                    url,
                    title: clean_html_text(&self.current_title),
                    snippet: clean_html_text(&self.current_snippet),
                    published: None,
                });
            }
        }
        self.current_title.clear();
        self.current_snippet.clear();
        self.in_h2 = false;
        self.in_title = false;
        self.in_snippet = false;
        self.snippet_done = false;
    }
}

impl HtmlSink for BingParser {
    fn on_start_tag(&mut self, name: &str, raw_attrs: &str, self_closing: bool) {
        if self_closing {
            return;
        }
        if name == "li" {
            if self.in_result {
                self.li_depth += 1;
            } else if let Some(cls) = find_attr_decoded(raw_attrs, "class") {
                if has_class_token(&cls, "b_algo") {
                    self.in_result = true;
                    self.li_depth = 1;
                    self.current_href = None;
                    self.current_title.clear();
                    self.current_snippet.clear();
                    self.snippet_done = false;
                }
            }
            return;
        }

        if !self.in_result {
            return;
        }

        match name {
            "h2" => self.in_h2 = true,
            "a" if self.in_h2 && self.current_href.is_none() => {
                self.current_href = find_attr_decoded(raw_attrs, "href").map(|h| h.into_owned());
                self.in_title = true;
            }
            "p" if !self.snippet_done => {
                self.in_snippet = true;
            }
            _ => {}
        }
    }

    fn on_end_tag(&mut self, name: &str) {
        if !self.in_result {
            return;
        }
        match name {
            "a" => self.in_title = false,
            "h2" => {
                self.in_h2 = false;
                self.in_title = false;
            }
            "p" if self.in_snippet => {
                self.in_snippet = false;
                if !self.current_snippet.trim().is_empty() {
                    self.snippet_done = true;
                }
            }
            "li" => {
                self.li_depth = self.li_depth.saturating_sub(1);
                if self.li_depth == 0 {
                    self.in_result = false;
                    self.flush_current();
                }
            }
            _ => {}
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.in_title {
            self.current_title.push_str(text);
        } else if self.in_snippet {
            self.current_snippet.push_str(text);
        }
    }
}

/// Extracts organic web search results from Bing HTML SERP responses.
pub(crate) fn parse_bing_html(html: &str) -> Vec<RawHit> {
    let mut parser = BingParser::new();
    scan_html(html, &mut parser);
    if parser.in_result {
        parser.flush_current();
    }
    parser.hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unwrap_ddg_and_bing_urls() {
        assert_eq!(
            unwrap_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=abc")
                .as_deref(),
            Some("https://rust-lang.org/")
        );
        assert!(unwrap_ddg_url("https://duckduckgo.com/y.js?ad_provider=bingv7aa").is_none());
        assert_eq!(
            unwrap_ddg_url("https://docs.rs/tokio").as_deref(),
            Some("https://docs.rs/tokio")
        );

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("https://tokio.rs/docs");
        let bing_ck = format!("https://www.bing.com/ck/a?!&&p=1&u=a1{b64}&ntb=1");
        assert_eq!(
            unwrap_bing_url(&bing_ck).as_deref(),
            Some("https://tokio.rs/docs")
        );
        assert!(unwrap_bing_url("https://www.bing.com/aclick?ld=xyz").is_none());
        assert!(unwrap_bing_url("javascript:void(0)").is_none());
    }

    #[test]
    fn test_parse_ddg_html_extracts_results_and_skips_ads() {
        let html = r##"
        <html><body>
        <div id="links" class="results">
          <div class="result results_links result--ad">
            <h2><a class="result__a" href="https://duckduckgo.com/y.js?ad_provider=foo">Ad Item</a></h2>
            <a class="result__snippet" href="#">Sponsored link</a>
          </div>
          <div class="result results_links results_links_deep web-result">
            <div class="links_main">
              <h2 class="result__title">
                <a class="result__a" href="/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=1">Rust &amp; Systems</a>
              </h2>
              <a class="result__snippet" href="/l/?uddg=https%3A%2F%2Frust-lang.org%2F">Reliable &amp; efficient software.</a>
            </div>
          </div>
          <div class="result results_links web-result">
            <h2 class="result__title">
              <a class="result__a" href="https://docs.rs/serde">Serde Docs</a>
            </h2>
            <div class="result__snippet">Serialization <b>framework</b> for Rust.</div>
          </div>
        </div>
        </body></html>"##;

        let hits = parse_ddg_html(html);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, SourceId::DuckDuckGo);
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert_eq!(hits[0].title.as_deref(), Some("Rust & Systems"));
        assert_eq!(
            hits[0].snippet.as_deref(),
            Some("Reliable & efficient software.")
        );
        assert_eq!(hits[1].rank, 2);
        assert_eq!(hits[1].url, "https://docs.rs/serde");
        assert_eq!(
            hits[1].snippet.as_deref(),
            Some("Serialization framework for Rust.")
        );
    }

    #[test]
    fn test_parse_bing_html_extracts_results_and_decodes_ck_redirects() {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("https://tokio.rs/tokio/tutorial");
        let html = format!(
            r#"
            <ol id="b_results">
              <li class="b_ad"><h2><a href="https://www.bing.com/aclick?ld=1">Ad</a></h2></li>
              <li class="b_algo">
                <h2><a href="https://www.bing.com/ck/a?!&amp;&amp;u=a1{b64}&amp;ntb=1">Tokio &quot;Tutorial&quot;</a></h2>
                <div class="b_caption">
                  <p><span class="news_dt">Nov 12, 2026</span> Asynchronous runtime for <strong>Rust</strong>.</p>
                </div>
              </li>
              <li class="b_algo">
                <ul><li>nested list item</li></ul>
                <h2><a href="https://crates.io/crates/tokio">tokio crate</a></h2>
                <p>Event-driven platform.</p>
              </li>
            </ol>"#
        );

        let hits = parse_bing_html(&html);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, SourceId::Bing);
        assert_eq!(hits[0].url, "https://tokio.rs/tokio/tutorial");
        assert_eq!(hits[0].title.as_deref(), Some("Tokio \"Tutorial\""));
        assert!(hits[0]
            .snippet
            .as_deref()
            .unwrap()
            .contains("Asynchronous runtime for Rust."));
        assert_eq!(hits[1].url, "https://crates.io/crates/tokio");
        assert_eq!(hits[1].snippet.as_deref(), Some("Event-driven platform."));
    }

    #[test]
    fn test_looks_like_captcha_detects_bot_blocks() {
        assert!(looks_like_captcha("<div class=\"anomaly-modal\">Please solve</div>"));
        assert!(looks_like_captcha("Our systems detected unusual traffic from your computer"));
        assert!(!looks_like_captcha("<ol id=\"b_results\"><li class=\"b_algo\">ok</li></ol>"));
    }

    #[test]
    fn test_parse_ddg_html_void_tags_in_snippet_do_not_leak_depth() {
        let html = r##"<html><body>
          <div class="result web-result">
            <h2 class="result__title"><a class="result__a" href="https://example.org/doc">Doc Title</a></h2>
            <a class="result__snippet" href="https://example.org/doc">First line<br>second<wbr>line</a>
            <div class="result__extras"><span class="result__url">example.org/doc</span></div>
          </div>
        </body></html>"##;

        let hits = parse_ddg_html(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet.as_deref(), Some("First line secondline"));
    }
}
