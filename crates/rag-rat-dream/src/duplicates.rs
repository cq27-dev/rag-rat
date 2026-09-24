//! `memory_duplicate` findings (#1445): pairs of live memories whose vectors sit close enough to be
//! the same note under other words. The pairs come from the caller — the vectors live in the
//! engine's embedding cache — and this module turns them into reviewable findings. Nothing is
//! merged: the reviewer merges and retires one, or dismisses the pair.
//!
//! No attempt is made to tell a restatement from a conflict by diffing numbers, negation, modal
//! verbs or identifiers between the two notes. On a real memory set (816 memories, 13 flagged
//! pairs) every pair differed in at least one of those, restatements included: paraphrases name
//! different symbols and cite different details, so the label carried no information.

use std::collections::{BTreeSet, HashMap};

use rusqlite::Connection;

use super::DreamFinding;
use super::findings::FindingKind;

/// Two live memories the caller found close in meaning. Order does not matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearDuplicate {
    pub memory_a: String,
    pub memory_b: String,
}

/// What the caller's comparison covered: the pairs at or above the threshold, and every memory it
/// actually compared (those with a vector under the active model). A memory missing from
/// `compared` — not yet embedded, or its vector reclaimed — was not evaluated, so a pair involving
/// it is carried, never resolved: a finding may only close on a comparison that ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NearDuplicates {
    pub pairs: Vec<NearDuplicate>,
    pub compared: BTreeSet<String>,
}

/// One `memory_duplicate` finding per pair whose memories are both live in the active repo, plus a
/// carried finding for every current one this run could not re-evaluate. The subject is the sorted
/// id pair and the evidence names both notes — never the similarity, so a reviewer's verdict
/// survives a re-embed that keeps the pair above the threshold, and a title edit re-opens it (even
/// before the edited note is re-embedded: the carried finding's evidence names the new title, and
/// the next run that compares both notes resolves it if they have drifted apart).
pub(super) fn near_duplicate_findings(
    conn: &Connection,
    near: &NearDuplicates,
) -> rusqlite::Result<Vec<DreamFinding>> {
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let live = rag_rat_query::memory::live_memory_status_sql("repo_memories");
    let titles: HashMap<String, String> = conn
        .prepare(&format!("SELECT id, title FROM repo_memories WHERE {live}{repo_clause}"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let finding = |first: &str, second: &str| {
        let (a, b) = (titles.get(first)?, titles.get(second)?);
        Some(DreamFinding {
            kind: FindingKind::MemoryDuplicate,
            subject: format!("{first}|{second}"),
            evidence: format!(
                "{first} {a:?} and {second} {b:?} are close in meaning: merge them into one and \
                 retire the other, or dismiss if both are needed [E0]"
            ),
            rank: FindingKind::MemoryDuplicate.base_rank(),
        })
    };
    let mut subjects = BTreeSet::new();
    let mut out = Vec::new();
    for pair in &near.pairs {
        let (first, second) = if pair.memory_a <= pair.memory_b {
            (&pair.memory_a, &pair.memory_b)
        } else {
            (&pair.memory_b, &pair.memory_a)
        };
        if first != second
            && let Some(found) = finding(first, second)
            && subjects.insert(found.subject.clone())
        {
            out.push(found);
        }
    }
    // Carry the current findings this run could not re-evaluate: both memories still live, but at
    // least one of them was not compared. A retired memory drops out of `titles`, so its pair is
    // not carried and resolves.
    let findings_clause =
        super::findings::dream_repo_scope_clause(&super::findings::dream_repo_scope(conn)?);
    let current: Vec<String> = conn
        .prepare(&format!(
            "SELECT subject FROM dream_findings WHERE kind = ?1 AND status IN {}{findings_clause}",
            super::findings::current_statuses_sql()
        ))?
        .query_map([FindingKind::MemoryDuplicate.as_db_str()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for subject in current {
        let Some((first, second)) = subject.split_once('|') else {
            continue;
        };
        let evaluated = near.compared.contains(first) && near.compared.contains(second);
        if !evaluated
            && !subjects.contains(&subject)
            && let Some(found) = finding(first, second)
        {
            subjects.insert(subject);
            out.push(found);
        }
    }
    Ok(out)
}
