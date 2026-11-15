//! Inbound peer eviction (Core `SelectNodeToEvict` / `AttemptToEvictConnection`).
//!
//! When inbound slots are full, accept a new peer only after disconnecting one
//! unprotected inbound. Protection mirrors Core: netgroup, recent blocks, recent
//! txs, lowest min-ping.

/// One inbound session considered for eviction.
#[derive(Clone, Debug)]
pub struct InboundEvictCandidate {
    pub id: u64,
    pub connected_at: u64,
    pub min_ping: Option<f64>,
    pub last_block: u64,
    pub last_tx: u64,
    pub netgroup: u64,
    pub noban: bool,
}

#[doc(hidden)]
pub const PROTECT_NETGROUP: usize = 4;
#[doc(hidden)]
pub const PROTECT_BLOCKS: usize = 4;
#[doc(hidden)]
pub const PROTECT_TXS: usize = 4;
#[doc(hidden)]
pub const PROTECT_MINPING: usize = 8;
/// Longest-connected inbound peers kept when slots are full.
#[doc(hidden)]
pub const PROTECT_LONGEST: usize = 8;

/// Pick one inbound id to disconnect, or `None` if every candidate is protected.
pub fn select_inbound_eviction(mut cands: Vec<InboundEvictCandidate>) -> Option<u64> {
    cands.retain(|c| !c.noban);
    if cands.is_empty() {
        return None;
    }

    protect_by_netgroup(&mut cands, PROTECT_NETGROUP);
    if cands.is_empty() {
        return None;
    }

    cands.sort_by(|a, b| {
        b.last_block
            .cmp(&a.last_block)
            .then_with(|| a.id.cmp(&b.id))
    });
    remove_first_k(&mut cands, PROTECT_BLOCKS);
    if cands.is_empty() {
        return None;
    }

    cands.sort_by(|a, b| b.last_tx.cmp(&a.last_tx).then_with(|| a.id.cmp(&b.id)));
    remove_first_k(&mut cands, PROTECT_TXS);
    if cands.is_empty() {
        return None;
    }

    cands.sort_by(|a, b| {
        ping_key(a.min_ping)
            .partial_cmp(&ping_key(b.min_ping))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    remove_first_k(&mut cands, PROTECT_MINPING);
    if cands.is_empty() {
        return None;
    }

    // A share of the longest-connected peers stays. Evicting them is how a
    // new inbound replaces the peers that have been useful the longest.
    cands.sort_by(|a, b| {
        a.connected_at
            .cmp(&b.connected_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    remove_first_k(&mut cands, PROTECT_LONGEST);
    // Newest peer in the largest netgroup. Group ids are the integers stored
    // at accept; this compares those integers and does not read asmap.
    let left = NonEmptyCands::new(&cands)?;
    Some(newest_in_largest_group(left))
}

/// A candidate slice that has at least one peer.
struct NonEmptyCands<'a>(&'a [InboundEvictCandidate]);

impl<'a> NonEmptyCands<'a> {
    fn new(cands: &'a [InboundEvictCandidate]) -> Option<Self> {
        if cands.is_empty() {
            None
        } else {
            Some(Self(cands))
        }
    }
}

/// Largest netgroup, ties to the smaller group id. Newest `connected_at`,
/// then the higher id, inside that group.
fn newest_in_largest_group(cands: NonEmptyCands<'_>) -> u64 {
    use std::collections::HashMap;
    struct GroupScore {
        count: usize,
        connected_at: u64,
        id: u64,
    }
    let mut groups: HashMap<u64, GroupScore> = HashMap::new();
    for c in cands.0 {
        groups
            .entry(c.netgroup)
            .and_modify(|g| {
                g.count += 1;
                if c.connected_at > g.connected_at
                    || (c.connected_at == g.connected_at && c.id > g.id)
                {
                    g.connected_at = c.connected_at;
                    g.id = c.id;
                }
            })
            .or_insert(GroupScore {
                count: 1,
                connected_at: c.connected_at,
                id: c.id,
            });
    }
    let mut best_group = cands.0[0].netgroup;
    let mut best_count = groups[&best_group].count;
    let mut best_id = groups[&best_group].id;
    for (group, score) in &groups {
        if score.count > best_count || (score.count == best_count && *group < best_group) {
            best_group = *group;
            best_count = score.count;
            best_id = score.id;
        }
    }
    best_id
}

#[doc(hidden)]
pub fn ping_key(min_ping: Option<f64>) -> f64 {
    min_ping.unwrap_or(f64::MAX)
}

#[doc(hidden)]
pub fn remove_first_k(cands: &mut Vec<InboundEvictCandidate>, k: usize) {
    let n = k.min(cands.len());
    cands.drain(0..n);
}

/// Protect up to `k` peers from the largest keyed netgroups (Core netgroup protect).
#[doc(hidden)]
pub fn protect_by_netgroup(cands: &mut Vec<InboundEvictCandidate>, k: usize) {
    use std::collections::HashMap;
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for c in cands.iter() {
        *counts.entry(c.netgroup).or_insert(0) += 1;
    }
    let mut groups: Vec<(u64, usize)> = counts.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut protect_ids = Vec::new();
    for (group, _) in groups {
        if protect_ids.len() >= k {
            break;
        }
        if let Some(c) = cands
            .iter()
            .filter(|c| c.netgroup == group)
            .min_by_key(|c| c.id)
        {
            protect_ids.push(c.id);
        }
    }
    cands.retain(|c| !protect_ids.contains(&c.id));
}

/// Stable netgroup key for eviction (IPv4 `/16`, IPv6 `/32`; no asmap).
pub fn eviction_netgroup(addr: std::net::SocketAddr) -> u64 {
    crate::netgroup::netgroup(addr, None)
}
