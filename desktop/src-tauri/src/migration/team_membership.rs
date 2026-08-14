//! Repair team↔member links that a membership edit failed to propagate.
//!
//! Two independent defects, both rooted in a team-membership change not
//! reaching the records that depend on it, are healed in one pass over
//! `teams.json` + `managed-agents.json`:
//!
//! 1. **Stale `persona_ids`.** Team records written before persona ids were
//!    namespaced hold bare slugs (`thufir`) instead of the namespaced id
//!    (`sietch-tabr:thufir`). Nothing rewrites them, and the interactive save
//!    path (`ensure_persona_ids_are_active`) *drops* an id it cannot resolve —
//!    silently shrinking the team. This migration rewrites a stale id to the
//!    persona it names whenever that persona is unambiguous, and — unlike the
//!    save path — never drops one it cannot resolve.
//!
//! 2. **Orphaned instance `team_id`.** Adding a standalone persona to an
//!    existing team records the persona on the team but does not backfill
//!    `team_id` on that persona's already-running instances. Team instructions
//!    are injected at spawn by matching `record.team_id`
//!    (`spawn_snapshot::effective_team_instructions`), so such an instance runs
//!    without them — a member in the roster but not in behavior. This backfills
//!    `team_id` on any instance whose persona is in a team but whose own
//!    `team_id` is unset.
//!
//! Both fixes are strictly additive (rewrite-or-leave, set-if-unset), so a
//! second boot is a clean no-op. Runs BEFORE `detach_directory_backed_teams`
//! so a not-yet-detached directory-backed team can still be scoped by its
//! `source_dir`, and before any UI save can drop an unresolvable id.

use std::collections::HashMap;
use std::path::Path;

use crate::managed_agents::{team_persona_key, ManagedAgentRecord, TeamRecord};

/// Repair stale team `persona_ids` and backfill orphaned instance `team_id`.
pub(super) fn repair_team_membership(app: &tauri::AppHandle) {
    let Ok(base_dir) = crate::managed_agents::managed_agents_base_dir(app) else {
        return;
    };
    match repair_team_membership_in_dir(&base_dir) {
        Ok(0) => {}
        Ok(n) => {
            eprintln!("buzz-desktop: team-membership-repair: repaired {n} record(s)")
        }
        Err(e) => eprintln!("buzz-desktop: team-membership-repair: {e}"),
    }
}

/// Core logic, decoupled from the Tauri `AppHandle` for testing.
///
/// `base_dir` is the managed-agents base directory (`<AppDataDir>/agents/`).
/// Returns the number of records changed across both files (0 = nothing to do,
/// nothing written, so a re-run is a clean no-op).
pub(super) fn repair_team_membership_in_dir(base_dir: &Path) -> Result<usize, String> {
    let teams_path = base_dir.join("teams.json");
    let agents_path = base_dir.join("managed-agents.json");

    // Definitions and teams both live in these two files; without either there
    // is nothing to link.
    if !teams_path.exists() || !agents_path.exists() {
        return Ok(0);
    }

    let teams_content = std::fs::read_to_string(&teams_path)
        .map_err(|e| format!("failed to read teams.json: {e}"))?;
    let mut teams: Vec<TeamRecord> = serde_json::from_str(&teams_content)
        .map_err(|e| format!("failed to parse teams.json: {e}"))?;

    let agents_content = std::fs::read_to_string(&agents_path)
        .map_err(|e| format!("failed to read managed-agents.json: {e}"))?;
    let mut agents: Vec<ManagedAgentRecord> = serde_json::from_str(&agents_content)
        .map_err(|e| format!("failed to parse managed-agents.json: {e}"))?;

    let rewrites = rewrite_stale_persona_ids(&mut teams, &agents);
    let backfills = backfill_instance_team_ids(&teams, &mut agents);

    if rewrites == 0 && backfills == 0 {
        return Ok(0);
    }

    // Pre-migration backups, taken ONCE per store (same contract as
    // `strip_baked_team_instructions`): a re-run after a partial failure must
    // not overwrite the pristine backup with a half-migrated snapshot. Both
    // captured before either write so a crash between the two writes still
    // leaves a full recovery pair.
    if rewrites > 0 {
        let bak = crate::util::resolved_backup_path(
            &teams_path,
            "teams.json.pre-team-membership-repair.bak",
        );
        crate::util::create_restricted_backup_once(&bak, teams_content.as_bytes())
            .map_err(|e| format!("failed to write teams.json backup: {e}"))?;
        let payload = serde_json::to_vec_pretty(&teams)
            .map_err(|e| format!("failed to serialize teams.json: {e}"))?;
        crate::managed_agents::atomic_write_json(&teams_path, &payload)?;
    }

    if backfills > 0 {
        let bak = crate::util::resolved_backup_path(
            &agents_path,
            "managed-agents.json.pre-team-membership-repair.bak",
        );
        crate::util::create_restricted_backup_once(&bak, agents_content.as_bytes())
            .map_err(|e| format!("failed to write managed-agents.json backup: {e}"))?;
        // Restricted: this store can carry plaintext agent nsecs on a
        // keyringless host (SECURITY.md:90).
        let payload = serde_json::to_vec_pretty(&agents)
            .map_err(|e| format!("failed to serialize managed-agents.json: {e}"))?;
        crate::managed_agents::atomic_write_json_restricted(&agents_path, &payload)?;
    }

    Ok(rewrites + backfills)
}

/// Set of persona ids that resolve to a definition — the definition records are
/// the key-less unified-store entries (`pubkey == ""`); their `slug` is the id
/// a team references.
fn resolvable_ids(agents: &[ManagedAgentRecord]) -> Vec<&str> {
    agents
        .iter()
        .filter(|r| r.pubkey.is_empty())
        .filter_map(|r| r.slug.as_deref())
        .collect()
}

/// Rewrite each team's stale `persona_ids` to the persona they name, when
/// unambiguous. Returns the number of ids rewritten.
///
/// An id is *stale* when no definition slug equals it. Its repair target is the
/// definition whose `source_team_persona_slug` equals the stale id — i.e. the
/// bare slug is the pre-namespacing form of that persona's namespaced slug. The
/// rewrite happens only when exactly one such definition exists (optionally
/// scoped to the team's source team); zero or many candidates leave the id
/// untouched, which is strictly safer than the save path that drops it.
fn rewrite_stale_persona_ids(teams: &mut [TeamRecord], agents: &[ManagedAgentRecord]) -> usize {
    let resolvable = resolvable_ids(agents);
    let definitions: Vec<&ManagedAgentRecord> =
        agents.iter().filter(|r| r.pubkey.is_empty()).collect();

    let mut rewritten = 0usize;
    for team in teams.iter_mut() {
        // Scope candidate personas to this team's source team when derivable:
        // a directory-backed team keys off its source_dir name; a detached team
        // keys off the unique source_team of its already-resolvable members.
        let scope = team_source_scope(team, &definitions);
        for id in team.persona_ids.iter_mut() {
            if resolvable.contains(&id.as_str()) {
                continue;
            }
            let candidates: Vec<&&ManagedAgentRecord> = definitions
                .iter()
                .filter(|d| d.source_team_persona_slug.as_deref() == Some(id.as_str()))
                .filter(|d| match scope.as_deref() {
                    Some(team_key) => d.source_team.as_deref() == Some(team_key),
                    None => true,
                })
                .collect();
            let [only] = candidates.as_slice() else {
                eprintln!(
                    "buzz-desktop: team-membership-repair: team {:?}: leaving unresolvable \
                     persona id {:?} ({} candidate(s))",
                    team.id,
                    id,
                    candidates.len()
                );
                continue;
            };
            if let Some(slug) = only.slug.as_deref() {
                *id = slug.to_string();
                rewritten += 1;
            }
        }
    }
    rewritten
}

/// The source-team key that scopes a team's persona candidates, or `None` when
/// it cannot be derived (matching then falls back to a global unique slug).
///
/// Directory-backed teams use `team_persona_key` (the pack manifest id). A
/// detached team (`source_dir` cleared) has no such key, so we infer it from
/// the unique `source_team` among its members that already resolve.
fn team_source_scope(team: &TeamRecord, definitions: &[&ManagedAgentRecord]) -> Option<String> {
    if team.source_dir.is_some() {
        return Some(team_persona_key(team).to_string());
    }
    let mut source_teams: Vec<&str> = team
        .persona_ids
        .iter()
        .filter_map(|id| {
            definitions
                .iter()
                .find(|d| d.slug.as_deref() == Some(id.as_str()))
                .and_then(|d| d.source_team.as_deref())
        })
        .collect();
    source_teams.sort_unstable();
    source_teams.dedup();
    match source_teams.as_slice() {
        [only] => Some((*only).to_string()),
        _ => None,
    }
}

/// Backfill `team_id` on instances whose persona is a team member but whose own
/// `team_id` is unset. Returns the number of instances updated.
///
/// Conservative: only sets an unset field, matching `detach.rs`. An instance
/// already bound to a team is never re-pointed, so a persona shared across two
/// teams keeps its existing binding.
fn backfill_instance_team_ids(teams: &[TeamRecord], agents: &mut [ManagedAgentRecord]) -> usize {
    // persona_id → team_id, first team wins for a persona in multiple teams
    // (deterministic by team order; the conservative is_none() guard below
    // means only unbound instances are touched anyway).
    let mut persona_to_team: HashMap<&str, &str> = HashMap::new();
    for team in teams {
        for persona_id in &team.persona_ids {
            persona_to_team
                .entry(persona_id.as_str())
                .or_insert(team.id.as_str());
        }
    }

    let mut backfilled = 0usize;
    for agent in agents.iter_mut() {
        if agent.pubkey.is_empty() || agent.team_id.is_some() {
            continue;
        }
        let Some(persona_id) = agent.persona_id.as_deref() else {
            continue;
        };
        if let Some(team_id) = persona_to_team.get(persona_id) {
            agent.team_id = Some((*team_id).to_string());
            backfilled += 1;
        }
    }
    backfilled
}

#[cfg(test)]
#[path = "team_membership_tests.rs"]
mod tests;
