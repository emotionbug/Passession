//! The Git Log lane graph: per row, the lane of the commit's node and the
//! edges crossing the row's top and bottom halves.
//!
//! Commits arrive in `--date-order` (children before parents). Walking them
//! in order, every lane waits for one commit: a commit takes the first lane
//! waiting for it (other lanes waiting for it converge into its node), its
//! first parent continues in that lane, and extra merge parents join a lane
//! already waiting for them or open a new one. Nothing looks ahead, so the
//! rows of a prefix never change when a later page is appended.

/// A line from lane `from` to lane `to` in one half of a row, coloured like `color`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub color: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// The node's lane.
    pub lane: usize,
    /// The node's colour index.
    pub color: usize,
    /// From the lane at the row's top to the lane at its centre.
    pub top: Vec<Edge>,
    /// From the lane at the row's centre to the lane at its bottom.
    pub bottom: Vec<Edge>,
    /// Lanes this row spans (the graph column width in lanes).
    pub lanes: usize,
}

#[derive(Clone, Debug)]
struct Lane {
    expects: String,
    color: usize,
}

/// The rows of `commits` (sha, parents) in response order.
pub fn build<'a, I>(commits: I) -> Vec<GraphRow>
where
    I: IntoIterator<Item = (&'a str, &'a [String])>,
{
    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut next_color = 0usize;
    let mut rows = Vec::new();
    for (sha, parents) in commits {
        let waiting: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, lane)| lane.as_ref().is_some_and(|l| l.expects == sha))
            .map(|(index, _)| index)
            .collect();
        let (lane, color) = match waiting.first() {
            Some(&index) => (index, lanes[index].as_ref().map_or(0, |l| l.color)),
            None => {
                let color = next_color;
                next_color += 1;
                let index = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                (index, color)
            }
        };
        let top_width = lanes.len();
        let mut top = Vec::new();
        for (index, slot) in lanes.iter().enumerate() {
            if let Some(l) = slot {
                let to = if waiting.contains(&index) { lane } else { index };
                top.push(Edge { from: index, to, color: l.color });
            }
        }
        // The node consumes every lane waiting for it.
        for &index in &waiting {
            lanes[index] = None;
        }
        if lane >= lanes.len() {
            lanes.resize(lane + 1, None);
        }
        let mut opened = Vec::new();
        let mut bottom = Vec::new();
        if let Some(first) = parents.first() {
            lanes[lane] = Some(Lane { expects: first.clone(), color });
            bottom.push(Edge { from: lane, to: lane, color });
        }
        for parent in parents.iter().skip(1) {
            if parent == parents.first().map_or("", String::as_str) {
                continue;
            }
            let existing = lanes
                .iter()
                .enumerate()
                .find(|(index, slot)| *index != lane && slot.as_ref().is_some_and(|l| &l.expects == parent))
                .map(|(index, slot)| (index, slot.as_ref().map_or(0, |l| l.color)));
            match existing {
                Some((index, lane_color)) => bottom.push(Edge { from: lane, to: index, color: lane_color }),
                None => {
                    let merge_color = next_color;
                    next_color += 1;
                    let index = lanes.iter().position(Option::is_none).unwrap_or(lanes.len());
                    if index == lanes.len() {
                        lanes.push(None);
                    }
                    lanes[index] = Some(Lane { expects: parent.clone(), color: merge_color });
                    opened.push(index);
                    bottom.push(Edge { from: lane, to: index, color: merge_color });
                }
            }
        }
        // Lanes passing this row keep their column.
        for (index, slot) in lanes.iter().enumerate() {
            if let Some(l) = slot
                && index != lane
                && !opened.contains(&index)
            {
                bottom.push(Edge { from: index, to: index, color: l.color });
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        let width = top_width.max(lanes.len()).max(lane + 1);
        rows.push(GraphRow { lane, color, top, bottom, lanes: width });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commits(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        list.iter()
            .map(|(sha, parents)| (sha.to_string(), parents.iter().map(|p| p.to_string()).collect()))
            .collect()
    }

    fn graph(list: &[(String, Vec<String>)]) -> Vec<GraphRow> {
        build(list.iter().map(|(sha, parents)| (sha.as_str(), parents.as_slice())))
    }

    fn e(from: usize, to: usize, color: usize) -> Edge {
        Edge { from, to, color }
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let list = commits(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        let rows = graph(&list);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], GraphRow { lane: 0, color: 0, top: vec![], bottom: vec![e(0, 0, 0)], lanes: 1 });
        assert_eq!(rows[1], GraphRow { lane: 0, color: 0, top: vec![e(0, 0, 0)], bottom: vec![e(0, 0, 0)], lanes: 1 });
        // The root commit ends its lane.
        assert_eq!(rows[2], GraphRow { lane: 0, color: 0, top: vec![e(0, 0, 0)], bottom: vec![], lanes: 1 });
    }

    #[test]
    fn branch_and_merge() {
        // m merges feature (f) into main (b); both fork from a.
        let list = commits(&[("m", &["b", "f"]), ("f", &["a"]), ("b", &["a"]), ("a", &[])]);
        let rows = graph(&list);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].bottom, vec![e(0, 0, 0), e(0, 1, 1)]);
        assert_eq!(rows[0].lanes, 2);
        // The feature commit sits in the opened lane with its colour.
        assert_eq!((rows[1].lane, rows[1].color), (1, 1));
        assert_eq!(rows[1].top, vec![e(0, 0, 0), e(1, 1, 1)]);
        assert_eq!(rows[1].bottom, vec![e(1, 1, 1), e(0, 0, 0)]);
        assert_eq!((rows[2].lane, rows[2].color), (0, 0));
        // Both lanes wait for a and converge into its node.
        assert_eq!((rows[3].lane, rows[3].color), (0, 0));
        assert_eq!(rows[3].top, vec![e(0, 0, 0), e(1, 0, 1)]);
        assert!(rows[3].bottom.is_empty());
        assert_eq!(rows[3].lanes, 2);
    }

    #[test]
    fn two_branch_tips_converge_into_one_commit() {
        let list = commits(&[("x", &["base"]), ("y", &["base"]), ("base", &[])]);
        let rows = graph(&list);
        assert_eq!((rows[0].lane, rows[0].color), (0, 0));
        // A new tip takes the first free lane with a new colour.
        assert_eq!((rows[1].lane, rows[1].color), (1, 1));
        assert_eq!(rows[1].top, vec![e(0, 0, 0)]);
        assert_eq!(rows[1].bottom, vec![e(1, 1, 1), e(0, 0, 0)]);
        assert_eq!(rows[2].lane, 0);
        assert_eq!(rows[2].top, vec![e(0, 0, 0), e(1, 0, 1)]);
        assert_eq!(rows[2].lanes, 2);
    }

    #[test]
    fn octopus_merge_opens_a_lane_per_extra_parent() {
        let list = commits(&[("o", &["p1", "p2", "p3"]), ("p3", &["r"]), ("p2", &["r"]), ("p1", &["r"]), ("r", &[])]);
        let rows = graph(&list);
        assert_eq!(rows[0].bottom, vec![e(0, 0, 0), e(0, 1, 1), e(0, 2, 2)]);
        assert_eq!(rows[0].lanes, 3);
        assert_eq!((rows[1].lane, rows[1].color), (2, 2));
        assert_eq!((rows[2].lane, rows[2].color), (1, 1));
        assert_eq!((rows[3].lane, rows[3].color), (0, 0));
        assert_eq!(rows[4].lane, 0);
        assert_eq!(rows[4].top, vec![e(0, 0, 0), e(1, 0, 1), e(2, 0, 2)]);
    }

    #[test]
    fn merge_parent_joins_a_lane_already_waiting_for_it() {
        // t (a tip) waits for a; m then merges a: no second lane opens for it.
        let list = commits(&[("t", &["a"]), ("m", &["b", "a"]), ("b", &["a"]), ("a", &[])]);
        let rows = graph(&list);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[1].bottom, vec![e(1, 1, 1), e(1, 0, 0), e(0, 0, 0)]);
        assert_eq!(rows[1].lanes, 2);
    }

    #[test]
    fn root_commit_frees_its_lane_and_trims_trailing_lanes() {
        // Two unrelated histories: the second root ends lane 1.
        let list = commits(&[("a2", &["a1"]), ("b2", &["b1"]), ("b1", &[]), ("a1", &[])]);
        let rows = graph(&list);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[2].lane, 1);
        assert_eq!(rows[2].bottom, vec![e(0, 0, 0)]);
        assert_eq!(rows[2].lanes, 2);
        assert_eq!(rows[3].lanes, 1);
        assert_eq!(rows[3].top, vec![e(0, 0, 0)]);
    }

    #[test]
    fn missing_parents_keep_their_lane_open() {
        // A filtered list: b's parent never appears, c starts a new lane.
        let list = commits(&[("b", &["gone"]), ("c", &["also-gone"])]);
        let rows = graph(&list);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[1].lane, 1);
        assert_eq!(rows[1].top, vec![e(0, 0, 0)]);
        assert_eq!(rows[1].bottom, vec![e(1, 1, 1), e(0, 0, 0)]);
        assert_eq!(rows[1].lanes, 2);
    }

    #[test]
    fn appending_a_page_keeps_earlier_rows() {
        let full = commits(&[
            ("m", &["b", "f"]),
            ("f", &["e"]),
            ("b", &["a"]),
            ("e", &["a"]),
            ("x", &["a"]),
            ("a", &["root"]),
            ("root", &[]),
        ]);
        let all = graph(&full);
        for split in 1..full.len() {
            let prefix = graph(&full[..split]);
            assert_eq!(prefix, all[..split].to_vec(), "prefix of {split} rows");
        }
    }
}
