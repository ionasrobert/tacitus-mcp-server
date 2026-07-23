use std::collections::HashSet;

use super::index::VaultIndex;
use crate::error::TacitusError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Links,
    Backlinks,
    Neighbors,
}

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub note_id: String,
    pub title: String,
}

/// The wikilink graph as a queryable API: outgoing links, backlinks, or
/// neighbors (both directions, BFS to `depth`).
pub fn graph_query(
    index: &VaultIndex,
    from: &str,
    relation: Relation,
    depth: usize,
) -> Result<Vec<GraphNode>, TacitusError> {
    if index.get(from).is_none() {
        return Err(TacitusError::new(
            "NOTE_NOT_FOUND",
            format!("No note with id \"{from}\"."),
            "Use list_notes to discover valid note ids.",
        ));
    }

    let ids: Vec<String> = match relation {
        Relation::Links => index.outgoing(from).iter().map(|n| n.id.clone()).collect(),
        Relation::Backlinks => index.backlinks(from).iter().map(|n| n.id.clone()).collect(),
        Relation::Neighbors => neighbors(index, from, depth.max(1)),
    };

    Ok(ids
        .iter()
        .filter_map(|id| {
            index.get(id).map(|n| GraphNode {
                note_id: n.id.clone(),
                title: n.title.clone(),
            })
        })
        .collect())
}

fn neighbors(index: &VaultIndex, from: &str, depth: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    seen.insert(from.to_string());
    let mut frontier = vec![from.to_string()];
    let mut collected = Vec::new();
    for _ in 0..depth {
        let mut next = Vec::new();
        for id in &frontier {
            let adjacent: Vec<String> = index
                .outgoing(id)
                .into_iter()
                .chain(index.backlinks(id))
                .map(|n| n.id.clone())
                .collect();
            for nid in adjacent {
                if seen.insert(nid.clone()) {
                    collected.push(nid.clone());
                    next.push(nid);
                }
            }
        }
        frontier = next;
    }
    collected
}
