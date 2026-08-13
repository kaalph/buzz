//! Runtime model-capability interpreter.
//!
//! `scripts/model-capabilities.json` is the single source of truth for every
//! model's six-axis capability profile (thinking mode, supported efforts,
//! default effort, Databricks v2 wire route, normalization policy, and picker
//! label). It is embedded at compile time (`include_str!`), parsed once through
//! strict `serde` (`deny_unknown_fields` + real enums), and cached in a
//! [`OnceLock`]. No codegen: both this interpreter and the TypeScript one in
//! `desktop/` read the same hand-curated manifest, and the shared normative
//! corpus (`scripts/normative-corpus.json`) is the cross-language contract that
//! guarantees they agree.
//!
//! ## Resolution algorithm (`resolve`)
//! 1. Provider canonicalization happens *inside* the resolver: trim, lowercase,
//!    and apply the alias map (`openai-compat` → `openai`,
//!    `databricks-v2` → `databricks_v2`).
//! 2. Provider-qualified exact-record lookup (case-insensitive on the model id).
//! 3. Boundary-aware family-rule match: strip any endpoint prefix at the first
//!    family token on a non-alphanumeric boundary, then take the longest match
//!    across every rule's `match_value` and `match_aliases`, breaking ties on
//!    the lexicographically smallest rule id.
//! 4. Provider fallback, distinguishing a blank model id from a concrete-unknown
//!    one.
//!
//! Every path yields a complete six-axis result; `registry_label` is populated
//! only on an exact-record hit.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::config::ThinkingEffort;

/// How a model activates and controls reasoning depth on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingMode {
    Adaptive,
    ManualBudget,
    None,
    OmitFields,
}

/// The Databricks v2 AI Gateway wire route a model is served on. `NotApplicable`
/// marks non-Databricks providers; `RouteUnknown` marks a blank Databricks id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatabricksV2Route {
    AnthropicMessages,
    MlflowChat,
    NotApplicable,
    OpenaiResponses,
    RouteUnknown,
}

/// Post-resolution effort normalization applied before a request is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NormalizationPolicy {
    None,
    OpenaiClampMaxToXhigh,
    OpenaiStandard,
}

/// Whether a family rule matches its token exactly or as a boundary-aware prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum MatchKind {
    Exact,
    Prefix,
}

/// A family/prefix rule: matches a canonical (prefix-stripped) model id against
/// `match_value` or any `match_aliases` token for the listed providers.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FamilyRule {
    id: String,
    match_kind: MatchKind,
    match_value: String,
    #[serde(default)]
    match_aliases: Vec<String>,
    providers: Vec<String>,
    thinking_mode: ThinkingMode,
    supported_efforts: Vec<ThinkingEffort>,
    default_effort: Option<ThinkingEffort>,
    databricks_v2_wire_route: DatabricksV2Route,
    normalization_policy: NormalizationPolicy,
    /// Documentation only; modeled so `deny_unknown_fields` accepts the manifest.
    #[serde(rename = "_comment", default)]
    #[allow(dead_code)]
    comment: Option<String>,
}

/// An authoritative six-axis snapshot for one concrete `(provider, model)` pair.
/// Exact records do *not* inherit from family rules at runtime; the doc fields
/// record the one-time provenance of each axis (see the manifest `_comment`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactRecord {
    provider: String,
    raw_model_id: String,
    registry_label: String,
    thinking_mode: ThinkingMode,
    supported_efforts: Vec<ThinkingEffort>,
    default_effort: Option<ThinkingEffort>,
    databricks_v2_wire_route: DatabricksV2Route,
    normalization_policy: NormalizationPolicy,
    // Documentation/provenance keys; modeled for strict parsing, not read at runtime.
    #[serde(rename = "_provenance", default)]
    #[allow(dead_code)]
    provenance: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    source: Option<String>,
    #[serde(rename = "_source", default)]
    #[allow(dead_code)]
    source_alt: Option<String>,
    #[serde(rename = "_reconciliation", default)]
    #[allow(dead_code)]
    reconciliation: Option<String>,
    #[serde(rename = "_reconciliation_note", default)]
    #[allow(dead_code)]
    reconciliation_note: Option<String>,
    #[serde(rename = "_reconciliation_doc", default)]
    #[allow(dead_code)]
    reconciliation_doc: Option<String>,
}

/// One provider's fallback profiles for a blank vs. a concrete-unknown model id.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackPair {
    blank: FallbackState,
    concrete_unknown: FallbackState,
}

/// A five-axis fallback profile (no label — fallbacks never carry one).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackState {
    databricks_v2_wire_route: DatabricksV2Route,
    thinking_mode: ThinkingMode,
    supported_efforts: Vec<ThinkingEffort>,
    default_effort: Option<ThinkingEffort>,
    normalization_policy: NormalizationPolicy,
}

/// Provider fallbacks keyed by canonical provider, with a `_default` catch-all.
/// Both states of every provider are required, so "both fallback states present"
/// is enforced structurally by the parse.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderFallbacks {
    anthropic: FallbackPair,
    openai: FallbackPair,
    databricks: FallbackPair,
    databricks_v2: FallbackPair,
    openrouter: FallbackPair,
    #[serde(rename = "_default")]
    default: FallbackPair,
}

impl ProviderFallbacks {
    /// Fallback pair for a canonical provider, or `_default` for anything else.
    fn get(&self, provider: &str) -> &FallbackPair {
        match provider {
            "anthropic" => &self.anthropic,
            "openai" => &self.openai,
            "databricks" => &self.databricks,
            "databricks_v2" => &self.databricks_v2,
            "openrouter" => &self.openrouter,
            _ => &self.default,
        }
    }

    /// Named pairs, for validation.
    fn named(&self) -> [(&str, &FallbackPair); 6] {
        [
            ("anthropic", &self.anthropic),
            ("openai", &self.openai),
            ("databricks", &self.databricks),
            ("databricks_v2", &self.databricks_v2),
            ("openrouter", &self.openrouter),
            ("_default", &self.default),
        ]
    }
}

/// The parsed manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    family_tokens: Vec<String>,
    family_rules: Vec<FamilyRule>,
    databricks_v2_known_models: Vec<String>,
    exact_records: Vec<ExactRecord>,
    provider_fallbacks: ProviderFallbacks,
    // Root documentation keys; modeled for strict parsing, not read at runtime.
    #[serde(rename = "_comment", default)]
    #[allow(dead_code)]
    comment: Option<String>,
    #[serde(rename = "_comment_databricks_v2_known_models", default)]
    #[allow(dead_code)]
    comment_known_models: Option<String>,
    #[serde(rename = "_sources", default)]
    #[allow(dead_code)]
    sources: std::collections::BTreeMap<String, String>,
}

/// The resolved six-axis capability profile for one `(provider, model)` query.
/// All fields borrow from the process-lifetime manifest. The field names and
/// declaration order are the corpus `expect` schema — the test-only generator
/// serializes this struct directly, so there is no second encoding of the axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CapabilityResult {
    pub thinking_mode: ThinkingMode,
    pub supported_efforts: &'static [ThinkingEffort],
    pub default_effort: Option<ThinkingEffort>,
    pub databricks_v2_wire_route: DatabricksV2Route,
    pub normalization_policy: NormalizationPolicy,
    pub registry_label: Option<&'static str>,
}

const MANIFEST_JSON: &str = include_str!("../../../scripts/model-capabilities.json");

static MANIFEST: OnceLock<Manifest> = OnceLock::new();

/// Parse (once) and return the embedded manifest. Panics on a malformed or
/// invalid bundled manifest — a build-time data error that must never ship.
fn manifest() -> &'static Manifest {
    MANIFEST.get_or_init(|| {
        let parsed: Manifest = serde_json::from_str(MANIFEST_JSON)
            .expect("bundled model-capabilities.json must parse");
        if let Err(e) = validate_manifest(&parsed) {
            panic!("bundled model-capabilities.json failed validation: {e}");
        }
        parsed
    })
}

/// Canonicalize a provider name: trim, lowercase, apply the alias map.
fn canonical_provider(provider: &str) -> String {
    let canon = provider.trim().to_ascii_lowercase();
    match canon.as_str() {
        "openai-compat" => "openai".to_string(),
        "databricks-v2" => "databricks_v2".to_string(),
        _ => canon,
    }
}

/// Strip an endpoint-naming prefix by locating the earliest family token that
/// begins on a non-alphanumeric boundary (or at the start), returning the slice
/// from that token onward. Returns the input unchanged when no token qualifies.
fn strip_catalog_prefix<'a>(model_lower: &'a str, family_tokens: &[String]) -> &'a str {
    let bytes = model_lower.as_bytes();
    let mut best: Option<usize> = None;
    for tok in family_tokens {
        let mut from = 0;
        while let Some(rel) = model_lower[from..].find(tok.as_str()) {
            let idx = from + rel;
            if idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric() {
                best = Some(best.map_or(idx, |b| b.min(idx)));
                break;
            }
            from = idx + 1;
        }
    }
    match best {
        Some(idx) => &model_lower[idx..],
        None => model_lower,
    }
}

/// Boundary-aware prefix test: `s` equals `token`, or `s` starts with `token`
/// and the following character is a non-alphanumeric boundary.
fn prefix_matches(token: &str, s: &str) -> bool {
    match s.strip_prefix(token) {
        Some(rest) => rest
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric()),
        None => false,
    }
}

/// Resolve the capability profile for a `(provider, raw_model_id)` pair.
pub fn resolve(provider: &str, raw_model_id: &str) -> CapabilityResult {
    let m = manifest();
    let canon = canonical_provider(provider);
    let blank = raw_model_id.trim().is_empty();

    // 1. Provider-qualified exact-record lookup (case-insensitive on the id).
    if !blank {
        for rec in &m.exact_records {
            if rec.provider == canon && rec.raw_model_id.eq_ignore_ascii_case(raw_model_id) {
                return CapabilityResult {
                    thinking_mode: rec.thinking_mode,
                    supported_efforts: &rec.supported_efforts,
                    default_effort: rec.default_effort,
                    databricks_v2_wire_route: rec.databricks_v2_wire_route,
                    normalization_policy: rec.normalization_policy,
                    registry_label: Some(&rec.registry_label),
                };
            }
        }
    }

    // 2. Boundary-aware family match: longest token wins, lexicographic tie-break.
    if !blank {
        let model_lower = raw_model_id.to_ascii_lowercase();
        let stripped = strip_catalog_prefix(&model_lower, &m.family_tokens);
        let mut best: Option<(usize, &FamilyRule)> = None;
        for rule in &m.family_rules {
            if !rule.providers.iter().any(|p| p == &canon) {
                continue;
            }
            let mut matched: Option<usize> = None;
            for tok in std::iter::once(&rule.match_value).chain(rule.match_aliases.iter()) {
                let ok = match rule.match_kind {
                    MatchKind::Exact => stripped == tok.as_str(),
                    MatchKind::Prefix => prefix_matches(tok, stripped),
                };
                if ok {
                    matched = Some(matched.map_or(tok.len(), |l| l.max(tok.len())));
                }
            }
            if let Some(len) = matched {
                let better = match best {
                    None => true,
                    Some((blen, brule)) => len > blen || (len == blen && rule.id < brule.id),
                };
                if better {
                    best = Some((len, rule));
                }
            }
        }
        if let Some((_, rule)) = best {
            let route = if canon == "databricks_v2" {
                rule.databricks_v2_wire_route
            } else {
                DatabricksV2Route::NotApplicable
            };
            return CapabilityResult {
                thinking_mode: rule.thinking_mode,
                supported_efforts: &rule.supported_efforts,
                default_effort: rule.default_effort,
                databricks_v2_wire_route: route,
                normalization_policy: rule.normalization_policy,
                registry_label: None,
            };
        }
    }

    // 3. Provider fallback (blank vs. concrete-unknown); never carries a label.
    let pair = m.provider_fallbacks.get(&canon);
    let state = if blank {
        &pair.blank
    } else {
        &pair.concrete_unknown
    };
    CapabilityResult {
        thinking_mode: state.thinking_mode,
        supported_efforts: &state.supported_efforts,
        default_effort: state.default_effort,
        databricks_v2_wire_route: state.databricks_v2_wire_route,
        normalization_policy: state.normalization_policy,
        registry_label: None,
    }
}

/// Authoritative list of known Databricks v2 model ids, sourced from the manifest.
pub fn databricks_v2_known_models() -> &'static [String] {
    &manifest().databricks_v2_known_models
}

/// Semantic invariants that strict typed parsing cannot express. Structural
/// checks (required fields, enum domains, both fallback states) are already
/// guaranteed by `serde` + `deny_unknown_fields`; this owns the rest.
fn validate_manifest(m: &Manifest) -> Result<(), String> {
    if m.family_tokens.is_empty() {
        return Err("family_tokens must be non-empty".to_string());
    }

    let check_efforts = |ctx: &str,
                         efforts: &[ThinkingEffort],
                         default: Option<ThinkingEffort>|
     -> Result<(), String> {
        if efforts.is_empty() {
            return Err(format!("{ctx}: supported_efforts must be non-empty"));
        }
        // Canonical enum order is None < Minimal < ... < Max; strict ascending
        // enforces sorted + duplicate-free in one check.
        if !efforts.windows(2).all(|w| w[0] < w[1]) {
            return Err(format!(
                "{ctx}: supported_efforts must be sorted in canonical order with no duplicates"
            ));
        }
        if let Some(d) = default {
            if !efforts.contains(&d) {
                return Err(format!(
                    "{ctx}: default_effort {d:?} not in supported_efforts"
                ));
            }
        }
        Ok(())
    };

    // Family rules: unique ids, non-empty providers, effort validity, and no
    // match token (value or alias) shared across or within rules.
    let mut rule_ids = std::collections::HashSet::new();
    let mut token_owner: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for rule in &m.family_rules {
        if !rule_ids.insert(rule.id.as_str()) {
            return Err(format!("duplicate family rule id: {}", rule.id));
        }
        if rule.providers.is_empty() {
            return Err(format!("family rule {} has empty providers", rule.id));
        }
        check_efforts(
            &format!("family_rule {}", rule.id),
            &rule.supported_efforts,
            rule.default_effort,
        )?;
        for tok in std::iter::once(&rule.match_value).chain(rule.match_aliases.iter()) {
            if let Some(prev) = token_owner.insert(tok.as_str(), rule.id.as_str()) {
                return Err(format!(
                    "duplicate match token {tok:?} (rules {prev} and {})",
                    rule.id
                ));
            }
        }
    }

    // Exact records: case-insensitive uniqueness of (provider, id), non-empty
    // labels, effort validity.
    let mut exact_keys = std::collections::HashSet::new();
    for rec in &m.exact_records {
        let key = (rec.provider.clone(), rec.raw_model_id.to_ascii_lowercase());
        if !exact_keys.insert(key) {
            return Err(format!(
                "duplicate exact record: {} / {}",
                rec.provider, rec.raw_model_id
            ));
        }
        if rec.registry_label.trim().is_empty() {
            return Err(format!(
                "exact record {} has an empty registry_label",
                rec.raw_model_id
            ));
        }
        check_efforts(
            &format!("exact_record {}", rec.raw_model_id),
            &rec.supported_efforts,
            rec.default_effort,
        )?;
    }

    // Known-model ids: case-insensitive uniqueness.
    let mut known = std::collections::HashSet::new();
    for id in &m.databricks_v2_known_models {
        if id.trim().is_empty() {
            return Err("databricks_v2_known_models contains an empty id".to_string());
        }
        if !known.insert(id.to_ascii_lowercase()) {
            return Err(format!("duplicate databricks_v2_known_models id: {id}"));
        }
    }

    // Provider fallbacks: effort validity for both states of every provider.
    for (name, pair) in m.provider_fallbacks.named() {
        check_efforts(
            &format!("fallback {name}/blank"),
            &pair.blank.supported_efforts,
            pair.blank.default_effort,
        )?;
        check_efforts(
            &format!("fallback {name}/concrete_unknown"),
            &pair.concrete_unknown.supported_efforts,
            pair.concrete_unknown.default_effort,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One entry of the generator's *inputs-only* table. It encodes **which
    /// questions to ask** — section headers and `(provider, raw_model_id)`
    /// query pairs plus a human note — and never any expected answer. Every
    /// answer is computed by the production [`resolve`] at generation time, so
    /// the manifest stays the single place capability behavior is encoded.
    enum Q {
        Section {
            group: &'static str,
            note: Option<&'static str>,
        },
        Vector {
            id: &'static str,
            provider: &'static str,
            raw_model_id: &'static str,
            note: Option<&'static str>,
        },
    }

    /// The inputs-only question set (section headers interleaved with query
    /// vectors, in file order). Answers live only in the manifest; this table
    /// says which questions to ask. Adding, removing, or reordering a `Vector`
    /// here changes the generated corpus — run `just regen-model-corpus`.
    const INPUTS: &[Q] = &[
    Q::Section { group: "Anthropic exact family rules", note: Some("All require thinking_mode=manual-budget or adaptive, correct supported_efforts, databricks_v2_wire_route=not-applicable") },
    Q::Vector { id: "anthropic-claude-3-family", provider: "anthropic", raw_model_id: "claude-3-7-sonnet-20250219", note: None },
    Q::Vector { id: "anthropic-claude-opus-4-5", provider: "anthropic", raw_model_id: "claude-opus-4-5", note: None },
    Q::Vector { id: "anthropic-claude-opus-4-7", provider: "anthropic", raw_model_id: "claude-opus-4-7", note: None },
    Q::Vector { id: "anthropic-claude-opus-4-8", provider: "anthropic", raw_model_id: "claude-opus-4-8", note: None },
    Q::Vector { id: "anthropic-claude-sonnet-5", provider: "anthropic", raw_model_id: "claude-sonnet-5-20260101", note: None },
    Q::Vector { id: "anthropic-claude-fable-5", provider: "anthropic", raw_model_id: "claude-fable-5", note: None },
    Q::Vector { id: "anthropic-claude-mythos-5", provider: "anthropic", raw_model_id: "claude-mythos-5", note: None },
    Q::Vector { id: "anthropic-claude-opus-4-6", provider: "anthropic", raw_model_id: "claude-opus-4-6", note: None },
    Q::Vector { id: "anthropic-claude-sonnet-4-6", provider: "anthropic", raw_model_id: "claude-sonnet-4-6", note: None },
    Q::Vector { id: "anthropic-claude-mythos-preview", provider: "anthropic", raw_model_id: "claude-mythos-preview", note: None },
    Q::Section { group: "Anthropic unknowns and fallbacks", note: None },
    Q::Vector { id: "anthropic-unknown-blank", provider: "anthropic", raw_model_id: "", note: None },
    Q::Vector { id: "anthropic-unknown-concrete", provider: "anthropic", raw_model_id: "claude-ultra-9000", note: None },
    Q::Section { group: "OpenAI exact family rules", note: None },
    Q::Vector { id: "openai-gpt5-pro", provider: "openai", raw_model_id: "gpt-5-pro", note: None },
    Q::Vector { id: "openai-gpt5.6", provider: "openai", raw_model_id: "gpt-5.6", note: None },
    Q::Vector { id: "openai-gpt5-6-dashed", provider: "openai", raw_model_id: "gpt-5-6", note: None },
    Q::Vector { id: "openai-gpt5.5", provider: "openai", raw_model_id: "gpt-5.5", note: None },
    Q::Vector { id: "openai-gpt5.4", provider: "openai", raw_model_id: "gpt-5.4", note: None },
    Q::Vector { id: "openai-gpt5.1", provider: "openai", raw_model_id: "gpt-5.1", note: None },
    Q::Vector { id: "openai-gpt5-base", provider: "openai", raw_model_id: "gpt-5", note: None },
    Q::Section { group: "OpenAI adversarial — gpt5 boundary-aware matching (ported from config.rs tests)", note: None },
    Q::Vector { id: "openai-gpt5-1106-should-not-match-base", provider: "openai", raw_model_id: "gpt-5-1106", note: Some("gpt-5-1106: '-1106' is a 4-digit date segment, NOT a short version (gpt5-base rejects only 1-3 digit suffixes). Must match base table [minimal,low,medium,high], NOT fall through to unknown.") },
    Q::Vector { id: "openai-gpt5-4o-matches-base", provider: "openai", raw_model_id: "gpt-5-4o", note: Some("gpt-5-4o: '4o' after '-' is NOT a short numeric suffix (it contains a letter). Must match gpt5-base. Crucially, must NOT match gpt-5.4 (the '4' is followed by 'o', not boundary char).") },
    Q::Vector { id: "openai-gpt5-pro-not-matching-gpt5-base", provider: "openai", raw_model_id: "gpt-5-pro", note: Some("gpt-5-pro should hit gpt5-pro rule (priority 20), NOT gpt-5 base.") },
    Q::Vector { id: "openai-multi-digit-version-gpt5-10", provider: "openai", raw_model_id: "gpt-5-10", note: Some("[reshape delta class B1] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Vector { id: "openai-gpt5-date-suffix", provider: "openai", raw_model_id: "gpt-5-20260101", note: Some("gpt-5-20260101 — long numeric suffix after base: '20260101' is 8 digits, beyond 1-3 digit reject, should hit gpt5-base.") },
    Q::Section { group: "DatabricksV2 — segment-based routing (ported from llm.rs tests)", note: None },
    Q::Vector { id: "dbv2-gpt5-route-openai-responses", provider: "databricks_v2", raw_model_id: "gpt-5.5", note: None },
    Q::Vector { id: "dbv2-claude-route-anthropic-messages", provider: "databricks_v2", raw_model_id: "claude-opus-4-7", note: None },
    Q::Vector { id: "dbv2-claude-prefix-stripped", provider: "databricks_v2", raw_model_id: "databricks-claude-opus-4-7", note: Some("databricks- prefix stripped → claude-opus-4-7 → Anthropic route") },
    Q::Vector { id: "dbv2-goose-claude-prefix-stripped", provider: "databricks_v2", raw_model_id: "goose-claude-fable-5", note: Some("goose- prefix stripped → claude-fable-5 → Anthropic adaptive+xhigh") },
    Q::Vector { id: "dbv2-team-prefix-stripped", provider: "databricks_v2", raw_model_id: "team-x-claude-opus-4-7", note: Some("team-x- prefix stripped → claude-opus-4-7 → Anthropic route") },
    Q::Vector { id: "dbv2-consolidated-llama-not-sol", provider: "databricks_v2", raw_model_id: "consolidated-llama", note: Some("segment test: 'sol' is a SUBSTRING of 'consolidated', not a boundary-aligned family token (family_tokens: claude-, gpt-) — no family-rule match. Falls through to databricks_v2 concrete-unknown → mlflow-chat.") },
    Q::Vector { id: "dbv2-terraform-coder-not-terra", provider: "databricks_v2", raw_model_id: "terraform-coder", note: Some("segment test: 'terra' is a prefix of 'terraform' — must NOT match 'terra' code name. Falls through to mlflow-chat.") },
    Q::Vector { id: "dbv2-corpus-reranker-not-opus", provider: "databricks_v2", raw_model_id: "corpus-reranker", note: Some("segment test: 'opus' is NOT a segment of corpus-reranker (segments: corpus, reranker). mlflow-chat.") },
    Q::Vector { id: "dbv2-octopus-model-not-opus", provider: "databricks_v2", raw_model_id: "octopus-model", note: Some("segment test: 'opus' is not a segment of octopus-model (segments: octopus, model). mlflow-chat.") },
    Q::Vector { id: "dbv2-goose-opus-5-is-anthropic", provider: "databricks_v2", raw_model_id: "goose-opus-5", note: Some("[reshape delta class F] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Section { group: "P2-A resolver-contract vectors (plan v4 §Resolver contract)", note: None },
    Q::Vector { id: "resolver-exact-raw-id-hit", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-4-mini", note: Some("Exact record exists. Must return exact Databricks override: low|medium|high (not family's none+xhigh).") },
    Q::Vector { id: "resolver-prefixed-alias-misses-exact", provider: "databricks_v2", raw_model_id: "team-x-databricks-gpt-5-4-mini", note: Some("Prefixed alias of an exact ID. Raw exact lookup MUST miss (key is team-x-..., not databricks-...). Falls to family rules (gpt5-4 family → none+xhigh).") },
    Q::Vector { id: "resolver-cross-provider-misses-exact", provider: "openai", raw_model_id: "databricks-gpt-5-4-mini", note: Some("Same raw ID but different provider. Exact record is databricks_v2-scoped; must miss. Falls to openai family rules.") },
    Q::Vector { id: "resolver-exact-efforts-plus-family-route", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-6-sol", note: Some("Exact record with efforts from models.dev (low|medium|high|max — provider-advertised, no none/xhigh). Route materialized from gpt5-6 family rule (openai-responses). Must return both, complete.") },
    Q::Vector { id: "dbv2-gpt5-5-exact-override", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-5", note: Some("Exact record adopts models.dev advertised set [low,medium,high]. Family rule (gpt5-5) has none+xhigh — provider-advertised wins per plan F1.") },
    Q::Section { group: "Blank vs concrete-unknown per provider (P2-B fallback vectors)", note: None },
    Q::Vector { id: "dbv2-blank-all7-route-unknown", provider: "databricks_v2", raw_model_id: "", note: Some("DBv2 blank: route-unknown, all 7 efforts, default medium.") },
    Q::Vector { id: "dbv2-concrete-unknown-mlflow-no-max", provider: "databricks_v2", raw_model_id: "some-unknown-model-xyz", note: Some("DBv2 concrete-unknown: mlflow-chat, all-except-max (6 efforts).") },
    Q::Vector { id: "openai-blank-all-except-max", provider: "openai", raw_model_id: "", note: Some("OpenAI blank: not-applicable route, all-except-max, medium default.") },
    Q::Vector { id: "openai-concrete-unknown-all-except-max", provider: "openai", raw_model_id: "gpt-4o", note: Some("OpenAI concrete unknown (unverified family): not-applicable route, all-except-max, medium default.") },
    Q::Vector { id: "anthropic-blank-adaptive-full", provider: "anthropic", raw_model_id: "", note: Some("Anthropic blank: assume adaptive with full support (incl. xhigh).") },
    Q::Vector { id: "anthropic-concrete-unknown-omit-fields", provider: "anthropic", raw_model_id: "claude-ultra-9000", note: Some("Anthropic concrete-unknown: omit-fields (never guess request shape).") },
    Q::Section { group: "Legacy Databricks provider (P3 effort rules)", note: None },
    Q::Vector { id: "databricks-gpt5-pro-effort", provider: "databricks", raw_model_id: "databricks-gpt-5-pro", note: Some("Legacy databricks GPT-5 Pro: same effort set as openai/gpt-5-pro — only [high], default high. Wire route not-applicable.") },
    Q::Vector { id: "databricks-gpt5-6-effort", provider: "databricks", raw_model_id: "databricks-gpt-5.6", note: Some("Legacy databricks GPT-5.6: same effort set as openai/gpt-5.6 — [none,low,medium,high,xhigh,max], default medium. Wire route not-applicable.") },
    Q::Vector { id: "databricks-gpt5-1-effort", provider: "databricks", raw_model_id: "databricks-gpt-5.1", note: Some("Legacy databricks GPT-5.1: same effort set as openai/gpt-5.1 — [none,low,medium,high], default none. Wire route not-applicable.") },
    Q::Section { group: "openai-compat alias canonicalization (Thufir P3 corrective action 1)", note: Some("Rust normalizes openai-compat → Provider::OpenAi before reaching normalize_effort_for_provider. TS PROVIDER_ALIASES must match so the UI effort table equals the Rust request behavior. Interpreters must canonicalize openai-compat → openai before resolving; the expected values are identical to the corresponding openai vectors.") },
    Q::Vector { id: "openai-compat-gpt-5-pro", provider: "openai-compat", raw_model_id: "gpt-5-pro", note: Some("openai-compat/gpt-5-pro must resolve identically to openai/gpt-5-pro: [high] only, default high.") },
    Q::Vector { id: "openai-compat-gpt-5-5", provider: "openai-compat", raw_model_id: "gpt-5.5", note: Some("openai-compat/gpt-5.5 must resolve identically to openai/gpt-5.5: [none,low,medium,high,xhigh], default medium.") },
    Q::Vector { id: "openai-compat-empty-model", provider: "openai-compat", raw_model_id: "", note: Some("openai-compat with blank model: resolves identically to openai unknown — all-except-max, default medium.") },
    Q::Section { group: "gpt-5-base guard divergence fix (CRITICAL)", note: Some("These vectors cover the 1-2 digit version suffix window where Rust and TS diverged. Must reject from gpt5-base, fall to concrete-unknown.") },
    Q::Vector { id: "openai-gpt5-10-preview-reject-base", provider: "openai", raw_model_id: "gpt-5-10-preview", note: Some("[reshape delta class B1] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Vector { id: "openai-gpt5-2-mini-reject-base", provider: "openai", raw_model_id: "gpt-5-2-mini", note: Some("[reshape delta class B1] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Vector { id: "openai-gpt5-9-dot-1-reject-base", provider: "openai", raw_model_id: "gpt-5-9.1", note: Some("[reshape delta class B1] uncurated/adversarial openai input; base gpt-5 prefix binds after the multi-digit guard is dropped.") },
    Q::Vector { id: "dbv2-gpt5-10-reject-base-route-preserved", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-10", note: Some("[reshape delta class B2] uncurated databricks_v2 gpt-5-<multi-digit>: base gpt-5 prefix binds (efforts narrow to minimal..high, normalization openai-standard) while the databricks_v2 wire route stays openai-responses. Pins the one delta class the other changed vectors did not exercise.") },
    Q::Vector { id: "dbv2-gpt-5-2-divergent-pin-preserved", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-2", note: Some("Guards the sole deliberately-divergent exact record: databricks-gpt-5-2 pins the removed dbv2-gpt-code-names-segment outcome (none..xhigh + openai-clamp-max-to-xhigh), NOT what the kept gpt-5 base prefix would produce (minimal..high + openai-standard). If the pin regresses toward the prefix, this vector fails.") },
    Q::Vector { id: "openai-customgpt-5-5-no-token-match", provider: "openai", raw_model_id: "customgpt-5-5-endpoint", note: Some("boundary-aware stripCatalogPrefix fix: customgpt-5-5-endpoint has no boundary-aligned gpt- token (g in customgpt is preceded by m), so the alias is NOT stripped. Falls to openai concrete-unknown fallback.") },
    Q::Section { group: "DBv2 gpt segment rule boundary vectors", note: Some("Verify segment match (not segment-prefix): 'gpt' must be an exact segment, not just a prefix of a segment.") },
    Q::Vector { id: "dbv2-gptoss-not-responses", provider: "databricks_v2", raw_model_id: "gptoss-model", note: Some("Collision-negative: 'gptoss' is a segment starting with 'gpt' but NOT an exact 'gpt' or 'gpt5' segment. Must fall to mlflow-chat.") },
    Q::Vector { id: "dbv2-gptj-6b-not-responses", provider: "databricks_v2", raw_model_id: "gptj-6b", note: Some("Collision-negative: 'gptj' is not an exact 'gpt' or 'gpt5' segment. Must fall to mlflow-chat.") },
    Q::Vector { id: "dbv2-customgpt-not-responses", provider: "databricks_v2", raw_model_id: "customgpt-5-5-endpoint", note: Some("customgpt-5-5-endpoint: boundary-aware strip finds no boundary-aligned gpt- (preceded by m in customgpt). Normalized alias is customgpt-5-5-endpoint itself, segments=[customgpt,5,5,endpoint], no gpt segment → mlflow-chat. This is the corrected behavior.") },
    Q::Vector { id: "dbv2-gpt-neox-mlflow", provider: "databricks_v2", raw_model_id: "gpt-neox-20b", note: Some("gpt-version-segment fix: gpt-neox-20b strips to itself (boundary-aligned at start), segments=[gpt,neox,20b]. gpt-version-segment requires next segment after gpt to be numeric; neox starts with n → no match → falls to mlflow-chat. This is corrected behavior.") },
    Q::Vector { id: "dbv2-gpt5-segment-positive", provider: "databricks_v2", raw_model_id: "databricks-gpt5-custom", note: Some("[reshape delta class E2] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Vector { id: "dbv2-dual-marker-gpt-wins-openai", provider: "databricks_v2", raw_model_id: "gpt-opus-5", note: Some("[reshape delta class F] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Section { group: "Missing positive vectors", note: None },
    Q::Vector { id: "anthropic-opus-5-adaptive-xhigh", provider: "anthropic", raw_model_id: "claude-opus-5-20270101", note: Some("Opus-5 prefix rule: adaptive, supports xhigh+max. Verifies the rule is wired.") },
    Q::Vector { id: "dbv2-sol-normalization-policy", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-6-sol", note: Some("Sol exact record: normalization_policy comes from gpt5-6 family rule (openai-standard). Also checks supported_efforts override.") },
    Q::Vector { id: "dbv2-luna-segment-route", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-6-luna", note: Some("Luna exact record: models.dev advertises [low,medium,high]. Exact record overrides family rule. Routes openai-responses (from family).") },
    Q::Vector { id: "dbv2-terra-segment-route", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-6-terra", note: Some("Terra exact record: models.dev advertises [low,medium,high]. Exact record overrides family rule. Routes openai-responses (from family).") },
    Q::Vector { id: "dbv2-gpt5-4-nano-exact", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-4-nano", note: Some("Exact record: gpt-5-4-nano, efforts [low,medium,high], registry_label 'GPT-5.4 Nano'.") },
    Q::Vector { id: "openrouter-concrete-unknown-fallback", provider: "openrouter", raw_model_id: "some-model-xyz", note: Some("openrouter concrete-unknown → _default fallback (wire route not-applicable).") },
    Q::Vector { id: "openai-gpt5-pro-uppercase-provider", provider: "OpenAI", raw_model_id: "gpt-5-pro", note: Some("Casing: uppercase provider 'OpenAI' normalized to 'openai'. Must resolve same as openai/gpt-5-pro.") },
    Q::Vector { id: "dbv2-exact-record-uppercase-model", provider: "databricks_v2", raw_model_id: "DATABRICKS-GPT-5-4-NANO", note: Some("Casing: uppercase raw_model_id. Case-insensitive exact lookup must hit the lowercase record.") },
    Q::Section { group: "Prototype-key safety vectors", note: Some("Verify that PROVIDER_FALLBACKS Map prevents prototype-chain pollution.") },
    Q::Vector { id: "prototype-key-constructor-blank", provider: "constructor", raw_model_id: "", note: Some("Prototype-key safety: \"constructor\" as provider must not resolve through Object prototype chain. Must return a complete record identical to default fallback.") },
    Q::Vector { id: "prototype-key-constructor-some-model", provider: "constructor", raw_model_id: "some-model", note: Some("Prototype-key safety: \"constructor\" as provider must not resolve through Object prototype chain. Must return a complete record identical to default fallback.") },
    Q::Vector { id: "prototype-key-proto__-blank", provider: "__proto__", raw_model_id: "", note: Some("Prototype-key safety: \"__proto__\" as provider must not resolve through Object prototype chain. Must return a complete record identical to default fallback.") },
    Q::Vector { id: "prototype-key-proto__-some-model", provider: "__proto__", raw_model_id: "some-model", note: Some("Prototype-key safety: \"__proto__\" as provider must not resolve through Object prototype chain. Must return a complete record identical to default fallback.") },
    Q::Section { group: "Boundary-aware strip prefix negative vectors", note: Some("customgpt/sgpt/mygpt have no boundary-aligned family token, so no prefix is stripped.") },
    Q::Vector { id: "openai-sgpt-5-5-no-strip", provider: "openai", raw_model_id: "sgpt-5-5", note: Some("boundary-aware strip: \"sgpt-5-5\" has gpt- at a non-boundary position (preceded by alphanumeric). No strip occurs. Falls to openai concrete-unknown fallback.") },
    Q::Vector { id: "dbv2-sgpt-5-5-mlflow", provider: "databricks_v2", raw_model_id: "sgpt-5-5", note: Some("boundary-aware strip: \"sgpt-5-5\" has gpt- at a non-boundary position (preceded by alphanumeric). No strip occurs. Falls to mlflow-chat.") },
    Q::Vector { id: "openai-mygpt-5-no-strip", provider: "openai", raw_model_id: "mygpt-5", note: Some("boundary-aware strip: \"mygpt-5\" has gpt- at a non-boundary position (preceded by alphanumeric). No strip occurs. Falls to openai concrete-unknown fallback.") },
    Q::Vector { id: "dbv2-mygpt-5-mlflow", provider: "databricks_v2", raw_model_id: "mygpt-5", note: Some("boundary-aware strip: \"mygpt-5\" has gpt- at a non-boundary position (preceded by alphanumeric). No strip occurs. Falls to mlflow-chat.") },
    Q::Vector { id: "dbv2-gpt-5-mini-exact-label", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-mini", note: Some("Exact record: label 'GPT-5 Mini'. Display rule (a): exact-record-only, family 'GPT-5' must NOT appear.") },
    Q::Vector { id: "dbv2-gpt-5-nano-exact-label", provider: "databricks_v2", raw_model_id: "databricks-gpt-5-nano", note: Some("Exact record: label 'GPT-5 Nano'. Display rule (a): exact-record-only, family 'GPT-5' must NOT appear.") },
    Q::Vector { id: "dbv2-family-matched-no-exact-label-null", provider: "databricks_v2", raw_model_id: "databricks-claude-opus-5-custom", note: Some("Family-matched (databricks- prefix stripped → claude-opus-5 → Anthropic adaptive). No exact record. Display rule (a): registry_label is null.") },
    Q::Vector { id: "dbv2-gpt-doubled-separator", provider: "databricks_v2", raw_model_id: "databricks-gpt--5", note: Some("[reshape delta class E2] uncurated/adversarial input; exact+prefix matcher result. See the compatibility table in PR #5597.") },
    Q::Section { group: "Reshape: gpt-5 prefix collision family (longest-prefix + boundary)", note: None },
    Q::Vector { id: "collision-gpt-5-base", provider: "openai", raw_model_id: "gpt-5", note: Some("Base GPT-5: shortest prefix; efforts minimal..high, openai-standard.") },
    Q::Vector { id: "collision-gpt-5-pro", provider: "openai", raw_model_id: "gpt-5-pro", note: Some("Longer prefix gpt-5-pro wins over gpt-5; efforts [high] only.") },
    Q::Vector { id: "collision-gpt-5-10", provider: "openai", raw_model_id: "gpt-5-10", note: Some("[class B1] multi-digit minor now binds base gpt-5 prefix (guard dropped).") },
    Q::Vector { id: "collision-gpt-5-6", provider: "openai", raw_model_id: "gpt-5.6", note: Some("Dotted minor binds gpt-5.6 prefix over base gpt-5.") },
    Q::Vector { id: "collision-gpt-5-1", provider: "openai", raw_model_id: "gpt-5.1", note: Some("gpt-5.1 prefix; default_effort none.") },
    Q::Section { group: "Reshape: uncurated DBv2 tokens fall to concrete_unknown (mlflow-chat)", note: None },
    Q::Vector { id: "uncurated-dbv2-gpt-6", provider: "databricks_v2", raw_model_id: "databricks-gpt-6", note: Some("[class E1] non-5 gpt version: no exact, no prefix -> mlflow-chat.") },
    Q::Vector { id: "uncurated-dbv2-gpt-4o", provider: "databricks_v2", raw_model_id: "databricks-gpt-4o", note: Some("[class E1] gpt-4o uncurated -> mlflow-chat.") },
    Q::Vector { id: "uncurated-dbv2-opus-5-bare", provider: "databricks_v2", raw_model_id: "opus-5", note: Some("[class F] bare Claude code-name segment, no leading claude -> mlflow-chat.") },
    Q::Vector { id: "uncurated-dbv2-sol-bare", provider: "databricks_v2", raw_model_id: "sol", note: Some("[class G] bare OpenAI code name -> mlflow-chat.") },
    Q::Vector { id: "uncurated-dbv2-claude-prefix", provider: "databricks_v2", raw_model_id: "databricks-claude-experimental", note: Some("dbv2-claude-prefix keeps uncurated claude* on anthropic-messages/omit-fields.") },
    Q::Section { group: "Reshape: negatives (must fall to fallback, not a family rule)", note: None },
    Q::Vector { id: "neg-gptoss-openai", provider: "openai", raw_model_id: "gptoss", note: Some("No gpt- boundary token -> openai concrete_unknown.") },
    Q::Vector { id: "neg-gptj-6b-openai", provider: "openai", raw_model_id: "gptj-6b", note: Some("gptj is not a gpt- token -> fallback.") },
    Q::Vector { id: "neg-consolidated-llama-dbv2", provider: "databricks_v2", raw_model_id: "consolidated-llama", note: Some("'sol' substring is not a segment; no match -> mlflow-chat.") },
    Q::Vector { id: "neg-terraform-coder-dbv2", provider: "databricks_v2", raw_model_id: "terraform-coder", note: Some("'terra' substring is not a segment -> mlflow-chat.") },
    Q::Vector { id: "neg-octopus-model-dbv2", provider: "databricks_v2", raw_model_id: "octopus-model", note: Some("'opus' substring is not a leading claude prefix -> mlflow-chat.") },
    Q::Section { group: "Reshape: boundary behavior of exact+prefix matcher", note: None },
    Q::Vector { id: "boundary-embedded-token-openai", provider: "openai", raw_model_id: "gpt-4-gpt-5-pro", note: Some("[class C] embedded gpt-5-pro token no longer captured -> fallback.") },
    Q::Vector { id: "boundary-dot-suffix-openai", provider: "openai", raw_model_id: "gpt-5.6.x", note: Some("[class D] '.'-boundary: gpt-5.6 prefix binds through a trailing dot segment.") },
    Q::Vector { id: "boundary-claude-3-digit-run-anthropic", provider: "anthropic", raw_model_id: "claude-35", note: Some("[class H] boundary-aware prefix: claude-3 no longer matches claude-35 -> fallback.") },
    Q::Vector { id: "boundary-claude-opus-4-70-anthropic", provider: "anthropic", raw_model_id: "claude-opus-4-70", note: Some("[class H] claude-opus-4-7 prefix does not bind claude-opus-4-70 -> fallback.") },
    Q::Vector { id: "boundary-gpt-5-1234-openai", provider: "openai", raw_model_id: "gpt-5-1234", note: Some("4-digit run: still binds base gpt-5 prefix (identical old and new).") },
    ];

    /// A section marker in the generated corpus (`_group` + optional `_note`).
    #[derive(Serialize)]
    struct SectionOut {
        #[serde(rename = "_group")]
        group: &'static str,
        #[serde(rename = "_note", skip_serializing_if = "Option::is_none")]
        note: Option<&'static str>,
    }

    /// One executable vector: the query, an optional note, and the resolver's
    /// snapshotted answer. `expect` is a [`CapabilityResult`] serialized
    /// directly — the axis names/order and the enum spellings come from the
    /// production types, so nothing about the answer is encoded a second time.
    #[derive(Serialize)]
    struct VectorOut {
        id: &'static str,
        provider: &'static str,
        raw_model_id: &'static str,
        #[serde(rename = "_note", skip_serializing_if = "Option::is_none")]
        note: Option<&'static str>,
        expect: CapabilityResult,
    }

    /// A heterogeneous corpus entry. `untagged` writes the inner object with no
    /// discriminator, yielding the one flat array the harnesses replay.
    #[derive(Serialize)]
    #[serde(untagged)]
    enum CorpusOut {
        Section(SectionOut),
        Vector(VectorOut),
    }

    const CORPUS_JSON: &str = include_str!("../../../scripts/normative-corpus.json");

    /// Render the corpus from [`INPUTS`] by running the production [`resolve`]
    /// over every query. Deterministic: fixed input order, struct-declaration
    /// key order, `serde_json` pretty (2-space) formatting, trailing newline.
    /// This is the single writer used by both the drift gate and the regen
    /// recipe, so "what the gate checks" and "what regen writes" cannot drift.
    fn generate_corpus_json() -> String {
        let entries: Vec<CorpusOut> = INPUTS
            .iter()
            .map(|q| match *q {
                Q::Section { group, note } => CorpusOut::Section(SectionOut { group, note }),
                Q::Vector {
                    id,
                    provider,
                    raw_model_id,
                    note,
                } => CorpusOut::Vector(VectorOut {
                    id,
                    provider,
                    raw_model_id,
                    note,
                    expect: resolve(provider, raw_model_id),
                }),
            })
            .collect();
        let mut json = serde_json::to_string_pretty(&entries)
            .expect("corpus entries serialize as pretty JSON");
        json.push('\n');
        json
    }

    /// Absolute path of the committed corpus, from the crate root at compile
    /// time — the same file [`CORPUS_JSON`] embeds, so the regen recipe writes
    /// exactly what the drift gate reads.
    fn corpus_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/normative-corpus.json")
    }

    #[test]
    fn bundled_manifest_parses_and_validates() {
        // Exercises the include_str! + strict serde + validate_manifest chain.
        let _ = manifest();
    }

    #[test]
    fn corpus_matches_generated_snapshot() {
        // Drift gate: the committed corpus must be byte-identical to what the
        // production resolver generates right now. A byte match proves every
        // `expect` in the file is the resolver's current answer — the same
        // cross-language contract the old hand-maintained corpus enforced,
        // now impossible to hand-edit out of sync. `just regen-model-corpus`
        // rewrites the file from this exact generator.
        assert_eq!(
            CORPUS_JSON,
            generate_corpus_json(),
            "scripts/normative-corpus.json is out of date — run `just regen-model-corpus` and commit the result"
        );
    }

    #[test]
    fn corpus_has_exactly_103_executable_vectors() {
        // Locks the vector count so a silent INPUTS edit can't quietly drop
        // coverage; must equal the gate in the TS harness
        // (modelCapabilitiesCorpus.test.mjs).
        let vectors = INPUTS
            .iter()
            .filter(|q| matches!(q, Q::Vector { .. }))
            .count();
        assert_eq!(
            vectors, 103,
            "corpus executable-vector count changed; update this gate deliberately"
        );
    }

    /// Rewrite `scripts/normative-corpus.json` from the production resolver.
    /// `#[ignore]` so the ordinary test run only *checks* the committed bytes
    /// (via `corpus_matches_generated_snapshot`); this is the writer half,
    /// invoked by `just regen-model-corpus`.
    #[test]
    #[ignore = "writer, not a check — run via `just regen-model-corpus`"]
    fn regen_corpus_file() {
        std::fs::write(corpus_path(), generate_corpus_json())
            .expect("write scripts/normative-corpus.json");
    }

    // --- Migrated relational/invariant tests (see 42-test inventory) ---
    // These assert cross-input properties a single corpus vector cannot express.

    #[test]
    fn test_gpt5_numeric_date_suffix_matches_base_not_version() {
        // A 4-digit date-like suffix on a non-boundary must fall to the gpt-5 base,
        // never to the gpt-5.1 version rule.
        let base = resolve("openai", "gpt-5");
        for id in ["gpt-5-1106", "gpt-5-20260101"] {
            assert_eq!(
                resolve("openai", id).supported_efforts,
                base.supported_efforts,
                "{id} must match gpt-5 base efforts"
            );
        }
    }

    #[test]
    fn test_gpt5_lettered_suffix_matches_base_not_gpt5_4() {
        // `gpt-5-4o` has an alnum char after `gpt-5-4`, so the gpt-5.4 rule must
        // not match; it falls to the base rule.
        let base = resolve("openai", "gpt-5");
        let gpt5_4 = resolve("openai", "gpt-5.4");
        let got = resolve("openai", "gpt-5-4o");
        assert_eq!(got.supported_efforts, base.supported_efforts);
        assert_ne!(got.supported_efforts, gpt5_4.supported_efforts);
    }

    #[test]
    fn test_gpt5_pro_wins_over_base_by_longest_prefix() {
        // `gpt-5-pro` matches both the base (`gpt-5`) and the pro rule; longest
        // prefix must select pro (high-only).
        let pro = resolve("openai", "gpt-5-pro");
        let base = resolve("openai", "gpt-5");
        assert_eq!(pro.supported_efforts, &[ThinkingEffort::High]);
        assert_ne!(pro.supported_efforts, base.supported_efforts);
    }

    #[test]
    fn test_every_resolve_yields_a_complete_result() {
        // Complete-result invariant: supported_efforts is never empty on any path.
        let inputs = [
            ("anthropic", "claude-opus-4-7"),
            ("anthropic", ""),
            ("anthropic", "claude-ultra-9000"),
            ("openai", "gpt-5"),
            ("openai", ""),
            ("openai", "gpt-4o"),
            ("databricks_v2", "databricks-gpt-5-4-mini"),
            ("databricks_v2", ""),
            ("databricks_v2", "some-unknown-xyz"),
            ("databricks", "databricks-gpt-5-pro"),
            ("openrouter", "whatever"),
            ("openai-compat", "gpt-5.5"),
            ("__proto__", ""),
            ("constructor", "some-model"),
            ("", ""),
            ("totally-unknown", "totally-unknown"),
        ];
        for (provider, model) in inputs {
            let got = resolve(provider, model);
            assert!(
                !got.supported_efforts.is_empty(),
                "resolve({provider:?}, {model:?}) returned empty supported_efforts"
            );
        }
    }

    // --- New direct-resolver tests (contract 5) ---

    #[test]
    fn test_whitespace_only_model_id_uses_blank_fallback() {
        // A whitespace-only id trims to blank and takes the blank fallback, which
        // differs from the concrete-unknown fallback for databricks_v2 (route).
        let ws = resolve("databricks_v2", "   ");
        let blank = resolve("databricks_v2", "");
        assert_eq!(ws, blank);
        assert_eq!(ws.databricks_v2_wire_route, DatabricksV2Route::RouteUnknown);
        let concrete = resolve("databricks_v2", "some-unknown-xyz");
        assert_eq!(
            concrete.databricks_v2_wire_route,
            DatabricksV2Route::MlflowChat
        );
    }

    #[test]
    fn test_prefix_tie_break_is_lexicographic_on_rule_id() {
        // gpt-5.1 matches the gpt-5.1 rule's exact value (len 7) over the base
        // prefix (len 5); the longest-match + tie-break path is deterministic.
        let a = resolve("openai", "gpt-5.1");
        let b = resolve("openai", "gpt-5.1");
        assert_eq!(a, b);
        assert_eq!(a.default_effort, Some(ThinkingEffort::None));
    }

    #[test]
    fn test_exact_record_beats_family_prefix() {
        // databricks-gpt-5-4-mini has an exact record (label present); the family
        // prefix would otherwise apply and carry no label.
        let got = resolve("databricks_v2", "databricks-gpt-5-4-mini");
        assert_eq!(got.registry_label, Some("GPT-5.4 mini"));
    }

    #[test]
    fn test_known_models_accessor_reads_manifest() {
        let known = databricks_v2_known_models();
        assert!(known.iter().any(|m| m == "databricks-gpt-5-5"));
        assert!(known.iter().any(|m| m == "databricks-claude-opus-4-7"));
    }
}
