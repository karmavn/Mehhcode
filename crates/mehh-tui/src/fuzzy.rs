//! Fuzzy matching and filtering utilities ported from TypeScript `packages/tui/src/fuzzy.ts`.
//! Matches if all query characters appear in order (not necessarily consecutive).
//! Lower score = better match.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

fn score_positions(text_chars: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match_index: isize = -1;
    let mut consecutive_matches: usize = 0;

    for &i in positions {
        // Reward consecutive matches
        if last_match_index == i as isize - 1 {
            consecutive_matches += 1;
            score -= (consecutive_matches as f64) * 5.0;
        } else {
            consecutive_matches = 0;
            // Penalize gaps
            if last_match_index >= 0 {
                score += ((i as isize - last_match_index - 1) as f64) * 2.0;
            }
        }

        // Reward word boundary matches
        if i == 0
            || (i > 0
                && matches!(
                    text_chars[i - 1],
                    ' ' | '\t' | '\n' | '\r' | '-' | '_' | '.' | '/' | ':'
                ))
        {
            score -= 10.0;
        }

        // Slight penalty for later matches
        score += (i as f64) * 0.1;

        last_match_index = i as isize;
    }

    score
}

fn score_contiguous(text_chars: &[char], start: usize, q_len: usize) -> f64 {
    let mut score = 0.0;
    let mut last_match_index: isize = -1;
    let mut consecutive_matches: usize = 0;

    for k in 0..q_len {
        let i = start + k;
        if last_match_index == i as isize - 1 {
            consecutive_matches += 1;
            score -= (consecutive_matches as f64) * 5.0;
        } else {
            consecutive_matches = 0;
        }

        if i == 0
            || matches!(
                text_chars[i - 1],
                ' ' | '\t' | '\n' | '\r' | '-' | '_' | '.' | '/' | ':'
            )
        {
            score -= 10.0;
        }

        score += (i as f64) * 0.1;
        last_match_index = i as isize;
    }

    score
}

fn match_prepared_chars(
    normalized_query: &str,
    query_chars: &[char],
    text_lower: &str,
    text_chars: &[char],
    positions: &mut Vec<usize>,
) -> FuzzyMatch {
    if query_chars.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }

    if query_chars.len() > text_chars.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    // Greedy alignment: each query character at its first occurrence after the previous one.
    positions.clear();
    let mut last_match_index: isize = -1;

    for &qc in query_chars {
        let start_idx = (last_match_index + 1) as usize;
        let found = text_chars[start_idx..]
            .iter()
            .position(|&tc| tc == qc)
            .map(|offset| start_idx + offset);

        match found {
            Some(idx) => {
                positions.push(idx);
                last_match_index = idx as isize;
            }
            None => {
                return FuzzyMatch {
                    matches: false,
                    score: 0.0,
                };
            }
        }
    }

    let mut score = score_positions(text_chars, positions);

    // The greedy alignment can scatter the query over an earlier, unrelated part of the text.
    // Also score every contiguous occurrence and keep the best alignment.
    let q_len = query_chars.len();
    for (start, window) in text_chars.windows(q_len).enumerate() {
        if window == query_chars {
            let contig_score = score_contiguous(text_chars, start, q_len);
            if contig_score < score {
                score = contig_score;
            }
        }
    }

    if normalized_query == text_lower {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

struct PreparedToken {
    lower: String,
    chars: Vec<char>,
    swapped: Option<(String, Vec<char>)>,
}

impl PreparedToken {
    fn new(token: &str) -> Self {
        let lower = token.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        let swapped = if let Some((letters, digits)) = split_letters_digits(&lower) {
            let s = format!("{digits}{letters}");
            let sc: Vec<char> = s.chars().collect();
            Some((s, sc))
        } else if let Some((digits, letters)) = split_digits_letters(&lower) {
            let s = format!("{letters}{digits}");
            let sc: Vec<char> = s.chars().collect();
            Some((s, sc))
        } else {
            None
        };
        Self {
            lower,
            chars,
            swapped,
        }
    }

    fn match_prepared(
        &self,
        text_lower: &str,
        text_chars: &[char],
        positions: &mut Vec<usize>,
    ) -> FuzzyMatch {
        let primary = match_prepared_chars(
            &self.lower,
            &self.chars,
            text_lower,
            text_chars,
            positions,
        );
        if primary.matches {
            return primary;
        }
        let Some((ref swapped_lower, ref swapped_chars)) = self.swapped else {
            return primary;
        };
        let swapped_match = match_prepared_chars(
            swapped_lower,
            swapped_chars,
            text_lower,
            text_chars,
            positions,
        );
        if !swapped_match.matches {
            return primary;
        }
        FuzzyMatch {
            matches: true,
            score: swapped_match.score + 5.0,
        }
    }
}

fn split_letters_digits(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let letter_count = bytes.iter().take_while(|b| b.is_ascii_lowercase()).count();
    if letter_count == 0 {
        return None;
    }
    let digit_count = bytes[letter_count..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digit_count > 0 && letter_count + digit_count == bytes.len() {
        Some((&s[..letter_count], &s[letter_count..]))
    } else {
        None
    }
}

fn split_digits_letters(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let digit_count = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let letter_count = bytes[digit_count..]
        .iter()
        .take_while(|b| b.is_ascii_lowercase())
        .count();
    if letter_count > 0 && digit_count + letter_count == bytes.len() {
        Some((&s[..digit_count], &s[digit_count..]))
    } else {
        None
    }
}

/// Matches if all characters in `query` appear in order in `text`.
/// Lower score = better match.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let token = PreparedToken::new(query);
    if token.chars.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    let text_lower = text.to_lowercase();
    let text_chars: Vec<char> = text_lower.chars().collect();
    let mut positions = Vec::with_capacity(token.chars.len());
    token.match_prepared(&text_lower, &text_chars, &mut positions)
}

/// Filter and sort items by fuzzy match quality, returning matched item indices and references.
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter_indexed<'a, T, S: AsRef<str>>(
    items: &'a [T],
    query: &str,
    get_text: impl Fn(&T) -> S,
) -> Vec<(usize, &'a T)> {
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        return items.iter().enumerate().collect();
    }

    let prepared_tokens: Vec<PreparedToken> = query_trimmed
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .map(PreparedToken::new)
        .collect();

    if prepared_tokens.is_empty() {
        return items.iter().enumerate().collect();
    }

    let mut results: Vec<(usize, &'a T, f64)> = Vec::new();
    let mut text_chars: Vec<char> = Vec::new();
    let mut positions: Vec<usize> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        let text = get_text(item);
        let text_lower = text.as_ref().to_lowercase();
        text_chars.clear();
        text_chars.extend(text_lower.chars());

        let mut total_score = 0.0;
        let mut all_match = true;

        for token in &prepared_tokens {
            let m = token.match_prepared(&text_lower, &text_chars, &mut positions);
            if m.matches {
                total_score += m.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((idx, item, total_score));
        }
    }

    results.sort_by(|a, b| {
        a.2.partial_cmp(&b.2)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    results.into_iter().map(|(idx, item, _)| (idx, item)).collect()
}

/// Filter and sort items by fuzzy match quality (best matches first).
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<'a, T, S: AsRef<str>>(
    items: &'a [T],
    query: &str,
    get_text: impl Fn(&T) -> S,
) -> Vec<&'a T> {
    fuzzy_filter_indexed(items, query, get_text)
        .into_iter()
        .map(|(_, item)| item)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_query_matches_everything_with_zero_score() {
        let result = fuzzy_match("", "anything");
        assert!(result.matches);
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn test_query_longer_than_text_does_not_match() {
        let result = fuzzy_match("longquery", "short");
        assert!(!result.matches);
    }

    #[test]
    fn test_exact_match_has_good_score() {
        let result = fuzzy_match("test", "test");
        assert!(result.matches);
        assert!(result.score < 0.0);
    }

    #[test]
    fn test_characters_must_appear_in_order() {
        let match_in_order = fuzzy_match("abc", "aXbXc");
        assert!(match_in_order.matches);

        let match_out_of_order = fuzzy_match("abc", "cba");
        assert!(!match_out_of_order.matches);
    }

    #[test]
    fn test_case_insensitive_matching() {
        let result = fuzzy_match("ABC", "abc");
        assert!(result.matches);

        let result2 = fuzzy_match("abc", "ABC");
        assert!(result2.matches);
    }

    #[test]
    fn test_consecutive_matches_score_better_than_scattered_matches() {
        let consecutive = fuzzy_match("foo", "foobar");
        let scattered = fuzzy_match("foo", "f_o_o_bar");

        assert!(consecutive.matches);
        assert!(scattered.matches);
        assert!(consecutive.score < scattered.score);
    }

    #[test]
    fn test_word_boundary_matches_score_better() {
        let at_boundary = fuzzy_match("fb", "foo-bar");
        let not_at_boundary = fuzzy_match("fb", "afbx");

        assert!(at_boundary.matches);
        assert!(not_at_boundary.matches);
        assert!(at_boundary.score < not_at_boundary.score);
    }

    #[test]
    fn test_scores_contiguous_occurrence_instead_of_earlier_scattered_alignment() {
        let opus = fuzzy_match("opus", "openrouter openrouter/anthropic/claude-opus-4.1");
        let sonnet = fuzzy_match("opus", "openrouter openrouter/anthropic/claude-sonnet-4.5");
        let gpt_oss = fuzzy_match("opus", "openrouter openrouter/openai/gpt-oss-120b");

        assert!(opus.score < sonnet.score);
        assert!(opus.score < gpt_oss.score);
    }

    #[test]
    fn test_uses_best_contiguous_occurrence_not_first() {
        let embedded_first = fuzzy_match("opus", "xxopus opus");
        let boundary_only = fuzzy_match("opus", "xxxxxx opus");

        assert!((embedded_first.score - boundary_only.score).abs() < 1e-6);
    }

    #[test]
    fn test_matches_swapped_alpha_numeric_tokens() {
        let result = fuzzy_match("codex52", "gpt-5.2-codex");
        assert!(result.matches);
    }

    #[test]
    fn test_filter_empty_query_returns_all_items_unchanged() {
        let items = vec!["apple", "banana", "cherry"];
        let result = fuzzy_filter(&items, "", |x| *x);
        assert_eq!(result, items.iter().collect::<Vec<_>>());
    }

    #[test]
    fn test_filter_filters_out_non_matching_items() {
        let items = vec!["apple", "banana", "cherry"];
        let result = fuzzy_filter(&items, "an", |x| *x);
        assert!(result.contains(&&"banana"));
        assert!(!result.contains(&&"apple"));
        assert!(!result.contains(&&"cherry"));
    }

    #[test]
    fn test_filter_sorts_results_by_match_quality() {
        let items = vec!["a_p_p", "app", "application"];
        let result = fuzzy_filter(&items, "app", |x| *x);
        assert_eq!(*result[0], "app");
    }

    #[test]
    fn test_filter_prioritizes_exact_matches_over_longer_prefix_matches() {
        let items = vec!["clone", "cl"];
        let result = fuzzy_filter(&items, "cl", |x| *x);
        assert_eq!(result, vec![&"cl", &"clone"]);
    }

    #[test]
    fn test_filter_ranks_contiguous_above_shared_provider_prefix() {
        let items = vec![
            "radius radius/gpt-5.4 radius gpt-5.4 GPT 5.4",
            "radius radius/agentic radius agentic Agentic",
            "radius radius/gpt-6-sol radius gpt-6-sol GPT 6 Sol",
            "radius radius/gpt-6-astra radius gpt-6-astra GPT 6 Astra",
        ];
        let result = fuzzy_filter(&items, "astra", |x| *x);
        assert_eq!(*result[0], items[3]);
    }

    #[test]
    fn test_filter_matches_slash_separated_queries_against_reordered_text() {
        struct Model {
            id: &'static str,
            provider: &'static str,
        }
        let items = vec![Model {
            id: "gpt-5.5",
            provider: "openai-codex",
        }];
        let result = fuzzy_filter(&items, "openai-codex/gpt-5.5", |m| {
            format!("{} {}", m.id, m.provider)
        });
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "gpt-5.5");
    }
}
