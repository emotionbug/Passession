//! Port of `@getpaseo/protocol/search/text-match` (Flutter `ui/command_center.dart`): match
//! tiers, Damerau-Levenshtein typo tolerance, multi-token field scoring and highlight
//! ranges. Offsets are char indices of the lower-cased text.

pub const TIER_EXACT: i64 = 0;
pub const TIER_WHOLE_WORD: i64 = 1;
pub const TIER_PREFIX: i64 = 2;
pub const TIER_WORD_START: i64 = 3;
pub const TIER_SUBSTRING: i64 = 4;
pub const TIER_SUBSEQUENCE: i64 = 5;
pub const TIER_FUZZY: i64 = 6;

/// One match: a tier plus where it landed. Lower is better on both, then on spread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchScore {
    pub tier: i64,
    pub offset: i64,
    pub spread: Option<i64>,
}

impl MatchScore {
    const fn new(tier: i64, offset: i64, spread: Option<i64>) -> Self {
        Self { tier, offset, spread }
    }
}

pub fn compare_scores(a: &MatchScore, b: &MatchScore) -> std::cmp::Ordering {
    a.tier
        .cmp(&b.tier)
        .then(a.offset.cmp(&b.offset))
        .then(a.spread.unwrap_or(0).cmp(&b.spread.unwrap_or(0)))
}

#[derive(Clone, Copy, Debug)]
struct FuzzyPolicy {
    max_edits: usize,
    transpositions_only: bool,
}

/// Letters and digits of any script: Hangul inside a word is not a boundary.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| !is_word_char(c))
}

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn find_from(text: &[char], query: &[char], from: usize) -> Option<usize> {
    if query.len() > text.len() {
        return None;
    }
    (from..=text.len() - query.len()).find(|&i| text[i..i + query.len()] == *query)
}

fn substring_match(query: &[char], text: &[char]) -> Option<MatchScore> {
    let mut best: Option<MatchScore> = None;
    let mut position = 0;
    while position + query.len() <= text.len() {
        let Some(found) = find_from(text, query, position) else { break };
        let before = (found > 0).then(|| text[found - 1]);
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
        let found_i = found as i64;
        if best.is_none_or(|b| tier < b.tier || (tier == b.tier && found_i < b.offset)) {
            best = Some(MatchScore::new(tier, found_i, None));
        }
        position = found + 1;
    }
    best
}

/// Characters in order within one whitespace-delimited word.
fn subsequence_match(query: &[char], text: &[char]) -> Option<MatchScore> {
    let mut qi = 0;
    let mut first: Option<usize> = None;
    let mut last = 0;
    for (index, c) in text.iter().enumerate() {
        if qi >= query.len() {
            break;
        }
        if c.is_whitespace() {
            qi = 0;
            first = None;
            continue;
        }
        if *c != query[qi] {
            continue;
        }
        if first.is_none() {
            first = Some(index);
        }
        last = index;
        qi += 1;
    }
    let first = first.filter(|_| qi == query.len())?;
    Some(MatchScore::new(TIER_SUBSEQUENCE, first as i64, Some((last - first + 1) as i64)))
}

/// Damerau-Levenshtein distance, abandoned once a whole row is over budget.
fn edit_distance(query: &[char], word: &[char], budget: usize) -> Option<usize> {
    if query.len().abs_diff(word.len()) > budget {
        return None;
    }
    let mut two_back: Vec<usize> = Vec::new();
    let mut previous: Vec<usize> = (0..=word.len()).collect();
    for q in 1..=query.len() {
        let mut current = vec![q];
        let mut row_best = q;
        for w in 1..=word.len() {
            let substitution = usize::from(query[q - 1] != word[w - 1]);
            let mut cost = (current[w - 1] + 1).min(previous[w] + 1).min(previous[w - 1] + substitution);
            if q > 1 && w > 1 && query[q - 1] == word[w - 2] && query[q - 2] == word[w - 1] {
                cost = cost.min(two_back[w - 2] + 1);
            }
            current.push(cost);
            row_best = row_best.min(cost);
        }
        if row_best > budget {
            return None;
        }
        two_back = previous;
        previous = current;
    }
    let distance = previous[word.len()];
    (distance <= budget).then_some(distance)
}

fn adjacent_transposition(query: &[char], word: &[char]) -> bool {
    if query.len() != word.len() {
        return false;
    }
    let mut index = 0;
    while index < query.len() && query[index] == word[index] {
        index += 1;
    }
    if index + 1 >= query.len() {
        return false;
    }
    query[index] == word[index + 1] && query[index + 1] == word[index] && query[index + 2..] == word[index + 2..]
}

/// Short tokens only forgive a swap; longer ones an edit or two.
fn fuzzy_policy(token: &[char]) -> Option<FuzzyPolicy> {
    match token.len() {
        0..=3 => None,
        4 => Some(FuzzyPolicy { max_edits: 1, transpositions_only: true }),
        5..=7 => Some(FuzzyPolicy { max_edits: 1, transpositions_only: false }),
        _ => Some(FuzzyPolicy { max_edits: 2, transpositions_only: false }),
    }
}

fn fuzzy_match(query: &[char], text: &[char], policy: FuzzyPolicy) -> Option<MatchScore> {
    if policy.max_edits == 0 || query.len() <= policy.max_edits {
        return None;
    }
    let mut best: Option<MatchScore> = None;
    let mut index = 0;
    while index < text.len() {
        if !is_word_char(text[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < text.len() && is_word_char(text[index]) {
            index += 1;
        }
        let word = &text[start..index];
        let candidates = [
            word,
            &word[..query.len().min(word.len())],
            &word[..(query.len() + policy.max_edits).min(word.len())],
        ];
        for (n, candidate) in candidates.iter().enumerate() {
            if candidates[..n].contains(candidate) {
                continue;
            }
            let distance = if policy.transpositions_only {
                adjacent_transposition(query, candidate).then_some(1)
            } else {
                edit_distance(query, candidate, policy.max_edits)
            };
            let Some(distance) = distance else { continue };
            let score = MatchScore::new(TIER_FUZZY, start as i64, Some(distance as i64));
            if best.is_none_or(|b| compare_scores(&score, &b).is_lt()) {
                best = Some(score);
            }
        }
    }
    best
}

fn score_match(query: &str, text: &str, fuzzy: Option<FuzzyPolicy>) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(MatchScore::new(TIER_EXACT, 0, None));
    }
    let q = chars(&query.to_lowercase());
    let t = chars(&text.to_lowercase());
    if q == t {
        return Some(MatchScore::new(TIER_EXACT, 0, None));
    }
    substring_match(&q, &t)
        .or_else(|| subsequence_match(&q, &t))
        .or_else(|| fuzzy.and_then(|policy| fuzzy_match(&q, &t, policy)))
}

fn tokenize(query: &str) -> Vec<String> {
    query.trim().to_lowercase().split_whitespace().map(str::to_owned).collect()
}

/// `scoreTextFields`: every token must land in some field; the result sums each token's
/// best tier, offset and spread. `None` filters the row out.
pub fn score_text_fields(query: &str, fields: &[&str], typo_tolerant: bool) -> Option<MatchScore> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Some(MatchScore::new(TIER_EXACT, 0, Some(0)));
    }
    let (mut tier, mut offset, mut spread) = (0, 0, 0);
    for token in tokens {
        let fuzzy = if typo_tolerant { fuzzy_policy(&chars(&token)) } else { None };
        let mut best: Option<MatchScore> = None;
        for field in fields {
            if let Some(score) = score_match(&token, field, fuzzy) && best.is_none_or(|b| compare_scores(&score, &b).is_lt()) {
                best = Some(score);
            }
        }
        let best = best?;
        tier += best.tier;
        offset += best.offset;
        spread += best.spread.unwrap_or(token.chars().count() as i64);
    }
    Some(MatchScore::new(tier, offset, Some(spread)))
}

fn match_ranges(query: &str, text: &str, score: &MatchScore) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = chars(&query.to_lowercase());
    let t = chars(&text.to_lowercase());
    let offset = score.offset.max(0) as usize;
    match score.tier {
        TIER_EXACT => vec![(0, t.len())],
        TIER_FUZZY => {
            let mut end = offset;
            while end < t.len() && is_word_char(t[end]) {
                end += 1;
            }
            vec![(offset, (end - offset).max(1))]
        }
        TIER_SUBSEQUENCE => {
            let mut ranges: Vec<(usize, usize)> = Vec::new();
            let mut qi = 0;
            for (index, c) in t.iter().enumerate().skip(offset) {
                if qi >= q.len() {
                    break;
                }
                if *c != q[qi] {
                    continue;
                }
                qi += 1;
                match ranges.last_mut() {
                    Some(last) if last.0 + last.1 == index => last.1 += 1,
                    _ => ranges.push((index, 1)),
                }
            }
            ranges
        }
        _ => vec![(offset, q.len())],
    }
}

/// `findHighlightRanges`: each token's best match in this field, merged `(start, len)` spans
/// in char indices.
pub fn highlight_ranges(query: &str, text: &str, typo_tolerant: bool) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for token in tokenize(query) {
        let fuzzy = if typo_tolerant { fuzzy_policy(&chars(&token)) } else { None };
        if let Some(score) = score_match(&token, text, fuzzy) {
            ranges.extend(match_ranges(&token, text, &score));
        }
    }
    ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.0 <= last.0 + last.1 => last.1 = last.1.max(range.0 + range.1 - last.0),
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_rank_like_the_protocol() {
        assert_eq!(score_match("main", "main", None).unwrap().tier, TIER_EXACT);
        assert_eq!(score_match("main", "the main branch", None).unwrap().tier, TIER_WHOLE_WORD);
        assert_eq!(score_match("ma", "main", None).unwrap().tier, TIER_PREFIX);
        assert_eq!(score_match("br", "main branch", None).unwrap().tier, TIER_WORD_START);
        assert_eq!(score_match("ai", "main", None).unwrap().tier, TIER_SUBSTRING);
        assert_eq!(score_match("mn", "main", None).unwrap().tier, TIER_SUBSEQUENCE);
        assert!(score_match("mn", "m n", None).is_none());
    }

    #[test]
    fn typo_tolerance_policies() {
        assert!(score_text_fields("setings", &["Settings"], true).is_some());
        assert_eq!(score_text_fields("setings", &["Settings"], false).map(|s| s.tier), Some(TIER_SUBSEQUENCE));
        assert_eq!(score_text_fields("tset", &["test"], true).map(|s| s.tier), Some(TIER_FUZZY));
        assert!(score_text_fields("abc def", &["abc"], false).is_none());
    }

    #[test]
    fn highlights_merge() {
        assert_eq!(highlight_ranges("new work", "New workspace", false), vec![(0, 3), (4, 4)]);
        assert_eq!(highlight_ranges("mn", "main", false), vec![(0, 1), (3, 1)]);
    }

    #[test]
    fn hangul_word_boundaries() {
        assert_eq!(score_match("스페", "워크스페이스", None).unwrap().tier, TIER_SUBSTRING);
        assert_eq!(score_match("워크", "워크스페이스", None).unwrap().tier, TIER_PREFIX);
        assert_eq!(score_match("설정", "앱 설정", None).unwrap().tier, TIER_WHOLE_WORD);
        assert_eq!(score_match("열기", "설정 열기창", None).unwrap().tier, TIER_WORD_START);
        let whole = score_text_fields("설정", &["설정 열기"], false).unwrap();
        let inner = score_text_fields("설정", &["환경설정"], false).unwrap();
        assert!(compare_scores(&whole, &inner).is_lt());
    }
}
