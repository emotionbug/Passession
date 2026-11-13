//! Command Center results (`command-center/results.ts`, `workspace-search.ts`,
//! `contributions.ts`, `list-metrics.ts` as ported in Flutter `ui/command_center.dart`):
//! contribution sections that reorder by match quality, pinned file/workspace/agent
//! sections, PR/MR number ranking and recency tie-breaks.

use std::cmp::Ordering;

use serde_json::Value;

use super::matching::{compare_scores, score_text_fields, MatchScore};
use crate::common::UiEffect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Workspace,
    Agent,
    File,
    Action,
}

/// What a chosen row does once the palette has closed.
#[derive(Clone, Debug)]
pub enum Run {
    /// A shell command (`onCommand: _execute`).
    Command(String, Value),
    Effect(UiEffect),
    /// `navigateToWorkspace` for (host, workspace).
    OpenWorkspace { host_id: String, workspace: Value },
    /// Toast an error instead of acting (e.g. no workspace path).
    Error(String),
}

#[derive(Clone, Debug)]
pub enum Icon {
    None,
    Glyph(&'static str),
    /// Project square with the workspace status bucket mark.
    Project { name: String, view_key: String, bucket: Option<crate::common::widgets::StatusBucket> },
    Provider { provider: String, bucket: Option<crate::common::widgets::StatusBucket> },
    File(String),
}

/// 0 when every token matched visible text, 1 when one only matched a hidden keyword.
#[derive(Clone, Copy, Debug)]
pub struct Score {
    pub field_rank: i64,
    pub matched: MatchScore,
}

#[derive(Clone, Debug)]
pub struct ResultRow {
    pub id: String,
    pub kind: Kind,
    pub title: String,
    pub subtitle: String,
    /// Matched but never rendered (an agent's cwd).
    pub hidden: Vec<String>,
    /// Activity time in ms since epoch.
    pub activity_at: Option<i64>,
    pub shortcut: String,
    pub icon: Icon,
    pub score: Option<Score>,
    pub change_request_number: Option<i64>,
    pub run: Run,
}

impl ResultRow {
    pub fn new(id: impl Into<String>, kind: Kind, title: impl Into<String>, run: Run) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            subtitle: String::new(),
            hidden: Vec::new(),
            activity_at: None,
            shortcut: String::new(),
            icon: Icon::None,
            score: None,
            change_request_number: None,
            run,
        }
    }

    pub fn two_line(&self) -> bool {
        matches!(self.kind, Kind::Workspace | Kind::Agent) || (self.kind == Kind::Action && !self.subtitle.is_empty())
    }
}

pub fn score_fields(query: &str, visible: &[&str], hidden: &[String]) -> Option<Score> {
    if let Some(matched) = score_text_fields(query, visible, false) {
        return Some(Score { field_rank: 0, matched });
    }
    if hidden.is_empty() {
        return None;
    }
    let mut all: Vec<&str> = visible.to_vec();
    all.extend(hidden.iter().map(String::as_str));
    score_text_fields(query, &all, false).map(|matched| Score { field_rank: 1, matched })
}

fn compare(a: &Score, b: &Score) -> Ordering {
    a.field_rank.cmp(&b.field_rank).then_with(|| compare_scores(&a.matched, &b.matched))
}

/// Built-in rows rank by which fields matched and the tier only; recency breaks ties.
fn compare_built_in(a: &Score, b: &Score) -> Ordering {
    a.field_rank.cmp(&b.field_rank).then(a.matched.tier.cmp(&b.matched.tier))
}

/// Most recent first; rows without a time sort after every row with one.
fn by_recency(a: &ResultRow, b: &ResultRow) -> Ordering {
    b.activity_at.unwrap_or(0).cmp(&a.activity_at.unwrap_or(0))
}

/// `filterAndRankBuiltInResults`: the five most recent rows with no query, else every match
/// by score then recency; ties keep input order.
pub fn rank_built_in(rows: Vec<ResultRow>, query: &str) -> Vec<ResultRow> {
    let mut indexed: Vec<(usize, ResultRow)> = rows.into_iter().enumerate().collect();
    if query.trim().is_empty() {
        indexed.sort_by(|a, b| by_recency(&a.1, &b.1).then(a.0.cmp(&b.0)));
        return indexed.into_iter().take(5).map(|(_, r)| r).collect();
    }
    let mut scored: Vec<(usize, ResultRow, Score)> = indexed
        .into_iter()
        .filter_map(|(i, row)| score_fields(query, &[&row.title, &row.subtitle], &row.hidden).map(|s| (i, row, s)))
        .collect();
    scored.sort_by(|a, b| compare_built_in(&a.2, &b.2).then_with(|| by_recency(&a.1, &b.1)).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(_, r, _)| r).collect()
}

/// An optional `pr`/`mr` word, optional `#`/`!`, then a number.
pub fn change_request_query(query: &str) -> Option<i64> {
    let lower = query.trim().to_lowercase();
    let rest = lower.strip_prefix("pr").or_else(|| lower.strip_prefix("mr")).map(str::trim_start).unwrap_or(&lower);
    let rest = rest.strip_prefix('#').or_else(|| rest.strip_prefix('!')).unwrap_or(rest);
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| rest.parse().ok()).flatten()
}

/// `filterAndRankWorkspaces`: a PR/MR number hit outranks any text match.
pub fn rank_workspaces(rows: Vec<ResultRow>, query: &str) -> Vec<ResultRow> {
    if query.trim().is_empty() {
        return rank_built_in(rows, query);
    }
    let number = change_request_query(query);
    let mut matches: Vec<(usize, ResultRow, Option<Score>, bool)> = rows
        .into_iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let score = score_fields(query, &[&row.title, &row.subtitle], &[]);
            let hit = number.is_some() && row.change_request_number == number;
            (score.is_some() || hit).then_some((i, row, score, hit))
        })
        .collect();
    matches.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| match (&a.2, &b.2) {
                (Some(x), Some(y)) => compare_built_in(x, y),
                _ => Ordering::Equal,
            })
            .then_with(|| by_recency(&a.1, &b.1))
            .then(a.0.cmp(&b.0))
    });
    matches.into_iter().map(|(_, r, _, _)| r).collect()
}

/// A registered action (presentation kind "action").
#[derive(Clone, Debug)]
pub struct Contribution {
    pub id: String,
    pub group: String,
    pub group_rank: i64,
    pub rank: i64,
    pub keywords: Vec<String>,
    /// Listed only once something has been typed.
    pub query_only: bool,
    pub title: String,
    pub section_title: String,
    pub icon: &'static str,
    pub shortcut: String,
    pub run: Run,
}

/// Registry order: group rank, group, rank, id.
pub fn compare_contributions(a: &Contribution, b: &Contribution) -> Ordering {
    a.group_rank.cmp(&b.group_rank).then_with(|| a.group.cmp(&b.group)).then(a.rank.cmp(&b.rank)).then_with(|| a.id.cmp(&b.id))
}

#[derive(Clone, Debug)]
pub struct Section {
    pub id: String,
    /// 0: contribution sections that reorder by match quality; 1: pinned below them.
    pub band: i64,
    pub rank: i64,
    pub title: Option<String>,
    pub results: Vec<ResultRow>,
    pub score: Option<Score>,
}

fn compare_sections(a: &Section, b: &Section) -> Ordering {
    a.band
        .cmp(&b.band)
        .then_with(|| match (&a.score, &b.score) {
            (Some(x), Some(y)) => compare(x, y),
            _ => Ordering::Equal,
        })
        .then(a.rank.cmp(&b.rank))
        .then_with(|| a.id.cmp(&b.id))
}

/// `buildContributionSections`: query-only rows drop out with no query; with one, the score
/// filters and both rows and sections sort by it.
pub fn contribution_sections(contributions: &[Contribution], query: &str) -> Vec<Section> {
    let has_query = !query.trim().is_empty();
    let mut sections: Vec<Section> = Vec::new();
    let mut registry: Vec<(String, usize)> = Vec::new();
    for (index, contribution) in contributions.iter().enumerate() {
        if contribution.query_only && !has_query {
            continue;
        }
        let score = if has_query {
            match score_fields(query, &[&contribution.title, ""], &contribution.keywords) {
                Some(score) => Some(score),
                None => continue,
            }
        } else {
            None
        };
        let position = match sections.iter().position(|s| s.id == contribution.group) {
            Some(position) => position,
            None => {
                sections.push(Section {
                    id: contribution.group.clone(),
                    band: 0,
                    rank: contribution.group_rank,
                    title: Some(contribution.section_title.clone()),
                    results: Vec::new(),
                    score: None,
                });
                sections.len() - 1
            }
        };
        registry.push((contribution.id.clone(), index));
        let section = &mut sections[position];
        let mut row = ResultRow::new(contribution.id.clone(), Kind::Action, contribution.title.clone(), contribution.run.clone());
        row.shortcut = contribution.shortcut.clone();
        row.icon = Icon::Glyph(contribution.icon);
        row.score = score;
        section.results.push(row);
        if let Some(score) = score && section.score.is_none_or(|s| compare(&score, &s).is_lt()) {
            section.score = Some(score);
        }
    }
    if !has_query {
        return sections;
    }
    let index_of = |id: &str| registry.iter().find(|(r, _)| r == id).map(|(_, i)| *i).unwrap_or(0);
    for section in &mut sections {
        section.results.sort_by(|a, b| {
            let delta = match (&a.score, &b.score) {
                (Some(x), Some(y)) => compare(x, y),
                _ => Ordering::Equal,
            };
            delta.then_with(|| index_of(&a.id).cmp(&index_of(&b.id)))
        });
    }
    sections.sort_by(compare_sections);
    sections
}

/// One projected list row (`projectCommandCenterRows` over `list-metrics.ts`).
#[derive(Clone, Debug)]
pub enum ListRow {
    Section { title: Option<String>, divider: bool },
    Result(Box<ResultRow>),
}

pub struct Projection {
    pub rows: Vec<ListRow>,
    /// Row index of each selectable result, in order.
    pub selectable: Vec<usize>,
}

pub fn project_rows(mut sections: Vec<Section>) -> Projection {
    sections.retain(|s| !s.results.is_empty());
    sections.sort_by(compare_sections);
    let mut rows = Vec::new();
    let mut selectable = Vec::new();
    for (index, section) in sections.into_iter().enumerate() {
        let divider = index > 0;
        if divider || section.title.is_some() {
            rows.push(ListRow::Section { title: section.title, divider });
        }
        for result in section.results {
            selectable.push(rows.len());
            rows.push(ListRow::Result(Box::new(result)));
        }
    }
    Projection { rows, selectable }
}

/// `_joinSubtitle`.
pub fn join_subtitle(parts: &[Option<&str>]) -> String {
    parts.iter().flatten().filter(|p| !p.trim().is_empty()).copied().collect::<Vec<_>>().join(" · ")
}

/// `describeWorkspaceFilePath`: (path, name, directory).
pub fn describe_file_path(path: &str) -> (String, String, String) {
    let normalized = path.replace('\\', "/");
    match normalized.rfind('/') {
        Some(index) => (normalized.clone(), normalized[index + 1..].to_owned(), normalized[..index].to_owned()),
        None => (normalized.clone(), normalized.clone(), String::new()),
    }
}

/// `resolveAgentLabel`: blank and the placeholder "New agent" have no label.
pub fn agent_label(title: &str) -> Option<String> {
    let trimmed = title.trim();
    (!trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("new agent")).then(|| trimmed.to_owned())
}

/// `pickAttentionAgent`: the top-level attention agent with the most urgent reason
/// (permission, error, finished), oldest first; the first listed wins a tie.
pub fn pick_attention_agent<'a>(agents: impl Iterator<Item = &'a Value>) -> Option<&'a Value> {
    use crate::features::workspace::rpc::str_of;
    let mut selected: Option<(i64, i64, &'a Value)> = None;
    for agent in agents {
        if agent.get("requiresAttention") != Some(&Value::Bool(true)) || !str_of(agent, "parentAgentId").is_empty() {
            continue;
        }
        let priority = match str_of(agent, "attentionReason") {
            "permission" => 0,
            "error" => 1,
            "finished" => 2,
            _ => continue,
        };
        let timestamp = super::parse_ms(agent.get("attentionTimestamp")).unwrap_or(i64::MAX);
        if selected.as_ref().is_none_or(|(p, t, _)| priority < *p || (priority == *p && timestamp < *t)) {
            selected = Some((priority, timestamp, agent));
        }
    }
    selected.map(|(_, _, agent)| agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str, at: i64) -> ResultRow {
        let mut row = ResultRow::new(id, Kind::Workspace, title, Run::Error(String::new()));
        row.activity_at = Some(at);
        row
    }

    #[test]
    fn built_in_rows_take_five_most_recent_without_query() {
        let rows = (0..7).map(|i| row(&i.to_string(), "x", i)).collect();
        let ranked = rank_built_in(rows, "");
        assert_eq!(ranked.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["6", "5", "4", "3", "2"]);
    }

    #[test]
    fn change_request_numbers_outrank_text() {
        assert_eq!(change_request_query("PR #12"), Some(12));
        assert_eq!(change_request_query("!7"), Some(7));
        assert_eq!(change_request_query("fix 12"), None);
        let mut hit = row("a", "other", 0);
        hit.change_request_number = Some(12);
        let text = row("b", "12 things", 10);
        let ranked = rank_workspaces(vec![text, hit], "12");
        assert_eq!(ranked[0].id, "a");
    }

    #[test]
    fn attention_priority_and_age() {
        let agents = [
            serde_json::json!({"id": "f", "requiresAttention": true, "attentionReason": "finished"}),
            serde_json::json!({"id": "e2", "requiresAttention": true, "attentionReason": "error", "attentionTimestamp": "2026-01-02T00:00:00Z"}),
            serde_json::json!({"id": "e1", "requiresAttention": true, "attentionReason": "error", "attentionTimestamp": "2026-01-01T00:00:00Z"}),
        ];
        assert_eq!(pick_attention_agent(agents.iter()), Some(&agents[2]));
    }

    #[test]
    fn attention_selection_preserves_identity_when_ids_collide() {
        let agents = [
            serde_json::json!({"id": "same", "requiresAttention": true, "attentionReason": "finished"}),
            serde_json::json!({"id": "same", "requiresAttention": true, "attentionReason": "permission"}),
        ];
        let chosen = pick_attention_agent(agents.iter()).unwrap();
        assert!(std::ptr::eq(chosen, &agents[1]));
    }
}
