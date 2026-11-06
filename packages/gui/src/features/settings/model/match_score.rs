//! Port of `@getpaseo/protocol/search/text-match` `scoreTextFields` (the
//! non-typo-tolerant path the settings pickers and catalog ranking use).

use std::cmp::Ordering;

const TIER_EXACT: i64 = 0;
const TIER_WHOLE_WORD: i64 = 1;
const TIER_PREFIX: i64 = 2;
const TIER_WORD_START: i64 = 3;
const TIER_SUBSTRING: i64 = 4;
const TIER_SUBSEQUENCE: i64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchScore {
    pub tier: i64,
    pub offset: i64,
    pub spread: Option<i64>,
}

pub fn compare(a: &MatchScore, b: &MatchScore) -> Ordering {
    a.tier
        .cmp(&b.tier)
        .then(a.offset.cmp(&b.offset))
        .then(a.spread.unwrap_or(0).cmp(&b.spread.unwrap_or(0)))
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

fn is_boundary(c: Option<char>) -> bool {
    c.map(|c| !is_word_char(c)).unwrap_or(true)
}

fn substring_match(query: &[char], text: &[char]) -> Option<MatchScore> {
    if query.len() > text.len() {
        return None;
    }
    let mut best: Option<MatchScore> = None;
    for found in 0..=text.len() - query.len() {
        if text[found..found + query.len()] != *query {
            continue;
        }
        let before = if found > 0 { Some(text[found - 1]) } else { None };
        let end = found + query.len();
        let after = text.get(end).copied();
        let starts = found == 0 || is_boundary(before);
        let ends = after.is_none() || is_boundary(after);
        let tier = if starts && ends {
            TIER_WHOLE_WORD
        } else if found == 0 {
            TIER_PREFIX
        } else if starts {
            TIER_WORD_START
        } else {
            TIER_SUBSTRING
        };
        let score = MatchScore { tier, offset: found as i64, spread: None };
        if best.map(|b| tier < b.tier || (tier == b.tier && (found as i64) < b.offset)).unwrap_or(true) {
            best = Some(score);
        }
    }
    best
}

/// Characters in order within one whitespace-delimited word.
fn subsequence_match(query: &[char], text: &[char]) -> Option<MatchScore> {
    let mut qi = 0;
    let mut first: i64 = -1;
    let mut last: i64 = -1;
    for (index, c) in text.iter().enumerate() {
        if qi >= query.len() {
            break;
        }
        if c.is_whitespace() {
            qi = 0;
            first = -1;
            last = -1;
            continue;
        }
        if *c != query[qi] {
            continue;
        }
        if first == -1 {
            first = index as i64;
        }
        last = index as i64;
        qi += 1;
    }
    if qi != query.len() || first == -1 {
        return None;
    }
    Some(MatchScore { tier: TIER_SUBSEQUENCE, offset: first, spread: Some(last - first + 1) })
}

fn score_match(query: &str, text: &str) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(MatchScore { tier: TIER_EXACT, offset: 0, spread: None });
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    if q == t {
        return Some(MatchScore { tier: TIER_EXACT, offset: 0, spread: None });
    }
    substring_match(&q, &t).or_else(|| subsequence_match(&q, &t))
}

/// Every whitespace token must land in some field; the result sums each
/// token's best tier, offset and spread. `None` filters the row out.
pub fn score_text_fields(query: &str, fields: &[&str]) -> Option<MatchScore> {
    let tokens: Vec<String> = query.trim().to_lowercase().split_whitespace().map(String::from).collect();
    if tokens.is_empty() {
        return Some(MatchScore { tier: TIER_EXACT, offset: 0, spread: Some(0) });
    }
    let (mut tier, mut offset, mut spread) = (0, 0, 0);
    for token in tokens {
        let mut best: Option<MatchScore> = None;
        for field in fields {
            if let Some(score) = score_match(&token, field)
                && best.map(|b| compare(&score, &b) == Ordering::Less).unwrap_or(true)
            {
                best = Some(score);
            }
        }
        let best = best?;
        tier += best.tier;
        offset += best.offset;
        spread += best.spread.unwrap_or(token.chars().count() as i64);
    }
    Some(MatchScore { tier, offset, spread: Some(spread) })
}

/// Input order without a query, else best match first (stable for ties).
pub fn rank<T: Clone>(items: &[T], query: &str, fields: impl Fn(&T) -> Vec<String>) -> Vec<T> {
    if query.trim().is_empty() {
        return items.to_vec();
    }
    let mut scored: Vec<(usize, MatchScore)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let values = fields(item);
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        if let Some(score) = score_text_fields(query, &refs) {
            scored.push((index, score));
        }
    }
    scored.sort_by(|a, b| compare(&a.1, &b.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(index, _)| items[index].clone()).collect()
}
