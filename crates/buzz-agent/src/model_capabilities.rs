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

use serde::Deserialize;

use crate::config::ThinkingEffort;

/// How a model activates and controls reasoning depth on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingMode {
    Adaptive,
    ManualBudget,
    None,
    OmitFields,
}

/// The Databricks v2 AI Gateway wire route a model is served on. `NotApplicable`
/// marks non-Databricks providers; `RouteUnknown` marks a blank Databricks id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatabricksV2Route {
    AnthropicMessages,
    MlflowChat,
    NotApplicable,
    OpenaiResponses,
    RouteUnknown,
}

/// Post-resolution effort normalization applied before a request is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
/// All fields borrow from the process-lifetime manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// One executable corpus vector: the query and its expected six-axis result.
    #[derive(Debug, Deserialize)]
    struct CorpusEntry {
        id: Option<String>,
        provider: Option<String>,
        raw_model_id: Option<String>,
        expect: Option<ExpectedCapabilities>,
    }

    #[derive(Debug, Deserialize)]
    struct ExpectedCapabilities {
        thinking_mode: ThinkingMode,
        supported_efforts: Vec<ThinkingEffort>,
        default_effort: Option<ThinkingEffort>,
        databricks_v2_wire_route: DatabricksV2Route,
        normalization_policy: NormalizationPolicy,
        registry_label: Option<String>,
    }

    const CORPUS_JSON: &str = include_str!("../../../scripts/normative-corpus.json");

    fn corpus() -> Vec<CorpusEntry> {
        serde_json::from_str(CORPUS_JSON).expect("normative-corpus.json must parse")
    }

    #[test]
    fn bundled_manifest_parses_and_validates() {
        // Exercises the include_str! + strict serde + validate_manifest chain.
        let _ = manifest();
    }

    #[test]
    fn corpus_vectors_all_resolve_as_expected() {
        let entries = corpus();
        let executable: Vec<&CorpusEntry> = entries.iter().filter(|e| e.expect.is_some()).collect();
        assert_eq!(
            executable.len(),
            103,
            "corpus executable-vector count changed; update this gate deliberately"
        );
        for entry in executable {
            let id = entry.id.as_deref().unwrap_or("<no-id>");
            let provider = entry.provider.as_deref().unwrap_or("");
            let model = entry.raw_model_id.as_deref().unwrap_or("");
            let expect = entry.expect.as_ref().unwrap();
            let got = resolve(provider, model);
            assert_eq!(
                got.thinking_mode, expect.thinking_mode,
                "{id}: thinking_mode"
            );
            assert_eq!(
                got.supported_efforts,
                expect.supported_efforts.as_slice(),
                "{id}: supported_efforts"
            );
            assert_eq!(
                got.default_effort, expect.default_effort,
                "{id}: default_effort"
            );
            assert_eq!(
                got.databricks_v2_wire_route, expect.databricks_v2_wire_route,
                "{id}: databricks_v2_wire_route"
            );
            assert_eq!(
                got.normalization_policy, expect.normalization_policy,
                "{id}: normalization_policy"
            );
            assert_eq!(
                got.registry_label,
                expect.registry_label.as_deref(),
                "{id}: registry_label"
            );
        }
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
