//! Pure domain core for query classification, URL canonicalization, RRF ranking,
//! and query-relevant Markdown passage extraction.

use std::collections::{HashMap, HashSet};

use reqwest::Url;

use crate::core::output::floor_char_boundary;
use crate::web::types::SearchHit;

const RRF_K: f64 = 60.0;
const MAX_STAGE1_SOURCES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SourceId {
    DuckDuckGo,
    Bing,
    Wikipedia,
    GitHub,
    CratesIo,
    Npm,
    StackOverflow,
}

impl SourceId {
    pub(crate) const ALL: [Self; 7] = [
        Self::DuckDuckGo,
        Self::Bing,
        Self::Wikipedia,
        Self::GitHub,
        Self::CratesIo,
        Self::Npm,
        Self::StackOverflow,
    ];

    #[inline]
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::DuckDuckGo => 0,
            Self::Bing => 1,
            Self::Wikipedia => 2,
            Self::GitHub => 3,
            Self::CratesIo => 4,
            Self::Npm => 5,
            Self::StackOverflow => 6,
        }
    }

    pub(crate) fn weight(self) -> f64 {
        match self {
            Self::DuckDuckGo | Self::Bing | Self::CratesIo | Self::Npm => 1.0,
            Self::GitHub | Self::StackOverflow => 0.95,
            Self::Wikipedia => 0.85,
        }
    }

    pub(crate) fn is_general_web(self) -> bool {
        matches!(self, Self::DuckDuckGo | Self::Bing)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Bing => "bing",
            Self::Wikipedia => "wikipedia",
            Self::GitHub => "github",
            Self::CratesIo => "crates.io",
            Self::Npm => "npm",
            Self::StackOverflow => "stackoverflow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawHit {
    pub source: SourceId,
    pub rank: usize,
    pub url: String,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub published: Option<String>,
}

/// Selects Stage 1 search sources for `query`: always `DuckDuckGo` + `Bing`, plus up to
/// 3 intent-matched official JSON APIs (total capped at `MAX_STAGE1_SOURCES`).
pub(crate) fn classify_query(query: &str) -> Vec<SourceId> {
    let lower = query.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '+' && c != '#')
        .filter(|w| !w.is_empty())
        .collect();

    let has_word = |candidates: &[&str]| words.iter().any(|w| candidates.contains(w));
    let has_phrase = |phrases: &[&str]| phrases.iter().any(|p| lower.contains(p));

    let mut sources = Vec::with_capacity(MAX_STAGE1_SOURCES);
    sources.push(SourceId::DuckDuckGo);
    sources.push(SourceId::Bing);

    let is_rust = has_word(&[
        "rust", "cargo", "crate", "crates", "crates.io", "tokio", "serde", "axum", "reqwest",
        "rustc", "clippy",
    ]);
    let is_js = has_word(&[
        "npm",
        "node",
        "nodejs",
        "typescript",
        "javascript",
        "react",
        "nextjs",
        "next.js",
        "vite",
        "bun",
        "pnpm",
        "yarn",
        "express",
        "tsconfig",
    ]);
    let is_repo = has_word(&[
        "github",
        "repo",
        "repository",
        "opensource",
        "library",
        "sdk",
        "cli",
        "release",
        "issues",
    ]) || has_phrase(&["open source", "pull request"]);
    let is_qa = has_word(&[
        "error",
        "exception",
        "panic",
        "cannot",
        "failed",
        "fix",
        "undefined",
        "segfault",
        "stackoverflow",
        "traceback",
    ]) || has_phrase(&["how to", "why does", "not working"])
        || words.iter().any(|w| w.starts_with("e0") && w.len() == 5);
    let is_ref = has_phrase(&["what is", "who is", "history of", "definition of"])
        || has_word(&["wikipedia", "overview", "meaning", "explain"]);

    if is_rust {
        sources.push(SourceId::CratesIo);
    }
    if is_js {
        sources.push(SourceId::Npm);
    }
    if is_repo {
        sources.push(SourceId::GitHub);
    }
    if is_qa && sources.len() < MAX_STAGE1_SOURCES {
        sources.push(SourceId::StackOverflow);
    }

    let no_dev_specialist = !is_rust && !is_js && !is_repo && !is_qa;
    if (is_ref || no_dev_specialist) && sources.len() < MAX_STAGE1_SOURCES {
        sources.push(SourceId::Wikipedia);
    }

    sources.truncate(MAX_STAGE1_SOURCES);
    sources
}

fn is_tracking_param(key: &str) -> bool {
    (key.len() >= 4 && key.as_bytes()[..4].eq_ignore_ascii_case(b"utm_"))
        || key.eq_ignore_ascii_case("fbclid")
        || key.eq_ignore_ascii_case("gclid")
        || key.eq_ignore_ascii_case("msclkid")
        || key.eq_ignore_ascii_case("mc_cid")
        || key.eq_ignore_ascii_case("mc_eid")
        || key.eq_ignore_ascii_case("ref")
        || key.eq_ignore_ascii_case("ref_src")
        || key.eq_ignore_ascii_case("igshid")
        || key.eq_ignore_ascii_case("si")
}

/// Normalizes an HTTP/HTTPS URL into a canonical deduplication key, stripping tracking params,
/// fragments, `www.` host prefix, and trailing slashes on non-root paths.
pub(crate) fn canonicalize_url(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let raw_host = url.host_str()?;
    let trimmed_host = raw_host.trim_end_matches('.');
    if trimmed_host.is_empty() {
        return None;
    }
    let strip_www = trimmed_host.len() > 4
        && trimmed_host.as_bytes()[..4].eq_ignore_ascii_case(b"www.")
        && trimmed_host[4..].contains('.');
    let without_www = if strip_www {
        &trimmed_host[4..]
    } else {
        trimmed_host
    };
    if raw_host != without_www || without_www.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower_host = without_www.to_ascii_lowercase();
        url.set_host(Some(&lower_host)).ok()?;
    }

    if url.scheme() != "https" {
        let _ = url.set_scheme("https");
    }
    url.set_fragment(None);

    let path = url.path();
    if path.is_empty() {
        url.set_path("/");
    } else if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let clean_path = if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        };
        url.set_path(&clean_path);
    }

    if url.query().is_some() {
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !is_tracking_param(k))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if pairs.is_empty() {
            url.set_query(None);
        } else {
            pairs.sort();
            let mut ser = url.query_pairs_mut();
            ser.clear();
            for (k, v) in &pairs {
                ser.append_pair(k, v);
            }
        }
    }

    Some(url.into())
}

struct MergedEntry {
    display_url: String,
    title: Option<String>,
    snippet: Option<String>,
    published: Option<String>,
    best_per_source: [usize; 7],
    best_rank: usize,
}

fn is_clean_single_spaced(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if bytes[0].is_ascii_whitespace() || bytes[bytes.len() - 1].is_ascii_whitespace() {
        return false;
    }
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if prev_space {
                return false;
            }
            prev_space = true;
        } else if ch.is_whitespace() {
            return false;
        } else {
            prev_space = false;
        }
    }
    true
}

fn clean_opt(value: Option<String>) -> Option<String> {
    let s = value?;
    if is_clean_single_spaced(&s) {
        return Some(s);
    }
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    (!out.is_empty()).then_some(out)
}

fn pick_richer(existing: Option<String>, candidate: Option<String>) -> Option<String> {
    match (existing, clean_opt(candidate)) {
        (None, cand) => cand,
        (Some(cur), Some(cand)) if cand.len() > cur.len() => Some(cand),
        (cur, _) => cur,
    }
}

/// Deduplicates raw multi-source hits by canonical URL and merges ranks with Reciprocal Rank Fusion.
pub(crate) fn fuse_and_rank(hits: Vec<RawHit>, max_results: usize) -> Vec<SearchHit> {
    if max_results == 0 || hits.is_empty() {
        return Vec::new();
    }

    let mut by_canon: HashMap<String, MergedEntry> = HashMap::with_capacity(hits.len());
    for raw in hits {
        let Some(canon) = canonicalize_url(&raw.url) else {
            continue;
        };
        let rank = raw.rank.max(1);
        let entry = by_canon.entry(canon).or_insert_with(|| MergedEntry {
            display_url: raw.url.trim().to_string(),
            title: None,
            snippet: None,
            published: None,
            best_per_source: [0; 7],
            best_rank: rank,
        });

        // Prefer https:// display URL when deduplicating http vs https hits.
        let trimmed_url = raw.url.trim();
        if !entry.display_url.starts_with("https://") && trimmed_url.starts_with("https://") {
            entry.display_url.clear();
            entry.display_url.push_str(trimmed_url);
        }
        entry.best_rank = entry.best_rank.min(rank);
        let slot = &mut entry.best_per_source[raw.source.index()];
        *slot = if *slot == 0 { rank } else { (*slot).min(rank) };

        entry.title = pick_richer(entry.title.take(), raw.title);
        entry.snippet = pick_richer(entry.snippet.take(), raw.snippet);
        if entry.published.is_none() {
            entry.published = clean_opt(raw.published);
        }
    }

    let mut scored: Vec<(f64, usize, String, SearchHit)> = Vec::with_capacity(by_canon.len());
    for (canon_key, entry) in by_canon {
        let mut rrf_score = 0.0f64;
        for source in SourceId::ALL {
            let rank = entry.best_per_source[source.index()];
            if rank > 0 {
                rrf_score += source.weight() / (RRF_K + (rank as f64));
            }
        }
        let hit = SearchHit {
            url: entry.display_url,
            title: entry.title,
            published: entry.published,
            content: entry.snippet,
        };
        scored.push((rrf_score, entry.best_rank, canon_key, hit));
    }

    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    scored
        .into_iter()
        .take(max_results)
        .map(|(_, _, _, hit)| hit)
        .collect()
}

fn is_stopword(tok: &str) -> bool {
    matches!(
        tok,
        "the" | "and" | "for" | "with" | "from" | "that" | "this" | "what" | "how" | "why"
            | "are" | "was" | "were" | "into" | "onto" | "about" | "your" | "can" | "not"
    )
}

pub(crate) fn tokenize_query_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.chars().nth(1).is_none() {
            continue;
        }
        let lower = raw.to_lowercase();
        if is_stopword(&lower) || !seen.insert(lower.clone()) {
            continue;
        }
        out.push(lower);
    }
    out
}

fn is_nav_boilerplate(block: &str) -> bool {
    let trimmed = block.trim();
    if trimmed.is_empty() {
        return true;
    }
    let mut lines = 0usize;
    let mut nav_lines = 0usize;
    for line in trimmed.lines() {
        lines += 1;
        let ts = line.trim_start();
        if ts.starts_with("- [") || ts.starts_with("* [") {
            nav_lines += 1;
        }
    }
    if lines >= 4 && nav_lines * 2 >= lines {
        return true;
    }
    let link_chars = trimmed.match_indices("](").count().saturating_mul(24);
    if link_chars > 0 && !trimmed.contains(". ") {
        let total_chars = trimmed.chars().count();
        if link_chars > total_chars {
            return true;
        }
    }
    false
}

fn score_block(query_terms: &[String], block: &str, heading: Option<&str>) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let block_lower = block.to_lowercase();
    let mut unique_hits = 0usize;
    let mut total_hits = 0usize;

    for term in query_terms {
        let count = block_lower.matches(term.as_str()).count();
        if count > 0 {
            unique_hits += 1;
            total_hits += count;
        }
    }

    if unique_hits == 0 {
        return 0.0;
    }

    let heading_bonus = heading
        .map(|h| {
            let hl = h.to_lowercase();
            query_terms
                .iter()
                .filter(|t| hl.contains(t.as_str()))
                .count() as f64
                * 1.5
        })
        .unwrap_or(0.0);

    let code_bonus = if block.contains("```") || block.contains('`') {
        0.5
    } else {
        0.0
    };

    (unique_hits as f64) * 3.0 + (total_hits as f64).ln_1p() + heading_bonus + code_bonus
}

fn push_formatted_block(out: &mut String, heading: Option<&str>, block: &str) {
    if let Some(h) = heading {
        if !block.starts_with('#') && !h.is_empty() {
            out.push_str("### ");
            out.push_str(h);
            out.push('\n');
        }
    }
    out.push_str(block);
}

fn block_char_len(heading: Option<&str>, block: &str) -> usize {
    let prefix_chars = match heading {
        Some(h) if !block.starts_with('#') && !h.is_empty() => 4 + h.chars().count() + 1,
        _ => 0,
    };
    prefix_chars + block.chars().count()
}

/// Extracts top query-relevant passages from rendered Markdown up to `max_chars` UTF-8 safe chars,
/// preserving document order and filtering navigation boilerplate.
#[cfg(test)]
pub(crate) fn extract_relevant_passages(
    query: &str,
    markdown: &str,
    max_chars: usize,
) -> Option<String> {
    let query_terms = tokenize_query_terms(query);
    extract_relevant_passages_with_terms(&query_terms, markdown, max_chars)
}

pub(crate) fn extract_relevant_passages_with_terms(
    query_terms: &[String],
    markdown: &str,
    max_chars: usize,
) -> Option<String> {
    let trimmed_md = markdown.trim();
    if trimmed_md.is_empty() || max_chars == 0 {
        return None;
    }

    struct Candidate<'a> {
        idx: usize,
        score: f64,
        heading: Option<&'a str>,
        block: &'a str,
    }

    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    let mut current_heading: Option<&str> = None;
    let mut has_scored_match = false;

    for (idx, raw_block) in trimmed_md.split("\n\n").enumerate() {
        let block = raw_block.trim();
        if block.is_empty() {
            continue;
        }
        if block.starts_with('#') && !block.contains('\n') {
            let h = block.trim_start_matches('#').trim();
            current_heading = (!h.is_empty()).then_some(h);
            continue;
        }
        if is_nav_boilerplate(block) {
            continue;
        }
        let s = score_block(query_terms, block, current_heading);
        let heading_for_block = if s > 0.0 {
            has_scored_match = true;
            current_heading.take()
        } else {
            current_heading = None;
            None
        };
        candidates.push(Candidate {
            idx,
            score: s,
            heading: heading_for_block,
            block,
        });
    }

    if candidates.is_empty() {
        return None;
    }

    if !has_scored_match {
        let mut fallback = String::with_capacity(max_chars.min(trimmed_md.len()));
        let mut fallback_chars = 0usize;
        for cand in candidates {
            let b_chars = cand.block.chars().count();
            if b_chars < 40 && fallback_chars > 0 {
                continue;
            }
            let sep = if fallback_chars == 0 { "" } else { "\n\n" };
            let sep_chars = if fallback_chars == 0 { 0 } else { 2 };
            let next_chars = fallback_chars + sep_chars + b_chars;
            if next_chars > max_chars {
                if fallback_chars == 0 {
                    let cut = byte_cut_chars(cand.block, max_chars.saturating_sub(3));
                    fallback.push_str(&cand.block[..cut]);
                    fallback.push_str("...");
                }
                break;
            }
            fallback.push_str(sep);
            fallback.push_str(cand.block);
            fallback_chars = next_chars;
        }
        return (!fallback.is_empty()).then_some(fallback);
    }

    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.idx.cmp(&b.idx)));

    let mut picked: Vec<(usize, Option<&str>, &str, Option<String>)> = Vec::new();
    let mut used_chars = 0usize;
    for cand in candidates {
        if cand.score <= 0.0 {
            break;
        }
        let sep_len = if picked.is_empty() { 0 } else { 7 }; // "\n\n...\n\n"
        let b_chars = block_char_len(cand.heading, cand.block);
        if used_chars + sep_len + b_chars <= max_chars {
            used_chars += sep_len + b_chars;
            picked.push((cand.idx, cand.heading, cand.block, None));
        } else if picked.is_empty() {
            let mut single = String::with_capacity(cand.block.len() + 32);
            push_formatted_block(&mut single, cand.heading, cand.block);
            let cut = byte_cut_chars(&single, max_chars.saturating_sub(3));
            let mut truncated = single[..cut].trim_end().to_string();
            truncated.push_str("...");
            picked.push((cand.idx, None, "", Some(truncated)));
            break;
        }
    }

    picked.sort_by_key(|(idx, _, _, _)| *idx);
    let mut out = String::with_capacity(max_chars.min(trimmed_md.len()) + 16);
    for (pos, (_, heading, block, owned_trunc)) in picked.into_iter().enumerate() {
        if pos > 0 {
            out.push_str("\n\n...\n\n");
        }
        if let Some(t) = owned_trunc {
            out.push_str(&t);
        } else {
            push_formatted_block(&mut out, heading, block);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn byte_cut_chars(s: &str, max_chars: usize) -> usize {
    if max_chars == 0 {
        return 0;
    }
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => floor_char_boundary(s, idx),
        None => s.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_query_routes_dev_and_general_sources() {
        let rust = classify_query("cargo tokio spawn_blocking tutorial");
        assert!(rust.contains(&SourceId::DuckDuckGo));
        assert!(rust.contains(&SourceId::Bing));
        assert!(rust.contains(&SourceId::CratesIo));
        assert!(rust.len() <= MAX_STAGE1_SOURCES);

        let js = classify_query("npm react router v7 migration");
        assert!(js.contains(&SourceId::Npm));
        assert!(!js.contains(&SourceId::CratesIo));

        let qa = classify_query("why does rust panic E0382 borrow error");
        assert!(qa.contains(&SourceId::StackOverflow));
        assert!(qa.contains(&SourceId::CratesIo));

        let general = classify_query("what is photosynthesis");
        assert_eq!(
            general,
            vec![SourceId::DuckDuckGo, SourceId::Bing, SourceId::Wikipedia]
        );
    }

    #[test]
    fn test_canonicalize_url_strips_tracking_www_fragment_and_rejects_non_http() {
        assert_eq!(
            canonicalize_url("http://www.Example.COM/docs/?utm_source=bing&b=2&a=1&fbclid=xyz#top")
                .as_deref(),
            Some("https://example.com/docs?a=1&b=2")
        );
        assert_eq!(
            canonicalize_url("https://rust-lang.org/#section").as_deref(),
            Some("https://rust-lang.org/")
        );
        assert!(canonicalize_url("javascript:alert(1)").is_none());
        assert!(canonicalize_url("file:///etc/passwd").is_none());
    }

    #[test]
    fn test_fuse_and_rank_rrf_boosts_consensus_and_merges_best_fields() {
        let hits = vec![
            RawHit {
                source: SourceId::DuckDuckGo,
                rank: 1,
                url: "https://only-ddg.example/a".into(),
                title: Some("Only DDG".into()),
                snippet: Some("Single source top hit".into()),
                published: None,
            },
            RawHit {
                source: SourceId::DuckDuckGo,
                rank: 2,
                url: "http://consensus.example/page/?utm_medium=cpc".into(),
                title: Some("Short".into()),
                snippet: Some("Short snippet".into()),
                published: None,
            },
            RawHit {
                source: SourceId::Bing,
                rank: 2,
                url: "https://www.consensus.example/page".into(),
                title: Some("Consensus Authoritative Guide".into()),
                snippet: Some("Much longer detailed snippet from Bing".into()),
                published: Some("2026-11-01".into()),
            },
        ];

        let ranked = fuse_and_rank(hits, 10);
        assert_eq!(ranked.len(), 2);
        // Consensus result appearing in both DDG and Bing at rank 2 outranks single-source rank 1.
        assert_eq!(ranked[0].url, "https://www.consensus.example/page");
        assert_eq!(
            ranked[0].title.as_deref(),
            Some("Consensus Authoritative Guide")
        );
        assert_eq!(
            ranked[0].content.as_deref(),
            Some("Much longer detailed snippet from Bing")
        );
        assert_eq!(ranked[0].published.as_deref(), Some("2026-11-01"));
    }

    #[test]
    fn test_extract_relevant_passages_selects_matching_blocks_and_respects_utf8_boundaries() {
        let md = "\
- [Home](/home)
- [Docs](/docs)
- [Pricing](/pricing)
- [Login](/login)

# Introduction

Welcome to our general portal page with unrelated marketing copy.

# Tokio Blocking Pool

Use `tokio::task::spawn_blocking` when running synchronous CPU work or blocking file I/O so Tokio worker threads never stall.

Tài liệu tiếng Việt hỗ trợ tìm kiếm nhanh chóng với `spawn_blocking` và xử lý UTF-8 an toàn.";

        let passage = extract_relevant_passages("tokio spawn_blocking", md, 240).unwrap();
        assert!(passage.contains("Tokio Blocking Pool"));
        assert!(passage.contains("spawn_blocking"));
        assert!(!passage.contains("Pricing"));

        // Ensure multi-byte Vietnamese truncation does not panic or slice mid-codepoint.
        let vi_only = "Hướng dẫn lập trình tiếng Việt rất chi tiết về bất đồng bộ trong Rust với Tokio runtime.";
        let cut = extract_relevant_passages("tiếng Việt Rust", vi_only, 30).unwrap();
        assert!(cut.chars().count() <= 30);
    }
}
