# Model-capability manifest: match-kind flatten compatibility table

This document accompanies `scripts/model-capabilities.json`. It records the behavior
delta from flattening the family-rule matcher's six `match_kind` variants down to two
(`exact` + `prefix`), and enumerates every input class whose resolution changes.

The reshape is **outcome-preserving for every curated model** and drops only uncurated
or adversarial inputs to the two provider fallbacks. Each drop is enumerated below with
its exact fallback destination — no delta is silent.

## What changed in the matcher

The prior manifest carried six match kinds; the reshaped manifest carries two.

| Removed `match_kind` | Prior rule(s) | What it did | Disposition |
|---|---|---|---|
| `gpt5-base` | `openai-gpt5-base` (`gpt-5`, `gpt5`) | `gpt-5` prefix match **guarded** against a bare multi-digit minor version (`gpt-5-10`, `gpt-5-2`): those fell through to the provider fallback rather than binding to base GPT-5. | Rule kept as `prefix` on token `gpt-5`; the multi-digit guard is **dropped** (see class **B1/B2**). |
| `gpt5-token` | `openai-gpt5-pro`, `-6`, `-5`, `-4`, `-1` | Matched the token anywhere on a left boundary with an end/`-` right boundary — so it caught the token **embedded** mid-string (`gpt-4-gpt-5-pro`) and treated `.`-suffixed forms as non-matches. | Rules kept as `prefix`; alias forms (`gpt5-pro`, `gpt-5-6`, `gpt5.6`, …) retained as `match_aliases`. Embedded-token capture is **dropped** (class **C**); `.`-boundary widened (class **D**). |
| `gpt-version-segment` | `dbv2-gpt-code-names-segment` (`gpt`, `gpt5`) | DBv2-only: routed any endpoint with a `gpt` segment followed by a numeric segment (`databricks-gpt-6`, `gpt-4o`, `databricks-gpt5-custom`) to `openai-responses`. | **Dropped.** Curated GPT endpoints are pinned as exact records; uncurated `gpt-*` DBv2 endpoints fall to the DBv2 `concrete_unknown` fallback → `mlflow-chat` (class **E1/E2**). Segment-anywhere is not expressible as a prefix. |
| `segment` (Claude names) | `dbv2-claude-code-names-segment` (`claude`, `opus`, `sonnet`, `haiku`, `mythos`, `fable`) | DBv2-only: routed any endpoint containing a Claude code-name **segment** (`goose-opus-5`, `opus-5`) to `anthropic-messages` with conservative `omit-fields`. | **Partially replaced.** New `dbv2-claude-prefix` rule (token `claude`) preserves routing for `databricks-claude-*` / `claude*` forms. Bare code-name segments with no leading `claude` (`opus-5`, `goose-opus-5`) are **dropped** to `mlflow-chat` (class **F**). |
| `segment` (OpenAI code names) | `dbv2-sol-luna-terra-segment` (`sol`, `luna`, `terra`) | DBv2-only: routed endpoints with a `sol`/`luna`/`terra` segment to `openai-responses`. | **Dropped.** Curated `databricks-gpt-5-6-{sol,luna,terra}` are pinned as exact records; bare `sol`/`luna`/`terra` segments fall to `mlflow-chat` (class **G**). |
| `segment-prefix` | *(none — unused)* | Declared in the matcher enum but no rule used it. | Removed with zero behavior impact. |

`prefix` in the reshaped matcher is **boundary-aware**: a token matches at string start only
when the character after it is end-of-string, `-`, or `.`. This is stricter than the prior
`prefix` kind's raw `startsWith`, which is itself a documented delta (class **H**).

## Curated-model immunity (the load-bearing invariant)

All 30 curated Databricks v2 records are materialized as complete six-axis **exact records**.
Exact lookup runs before any prefix matching and returns the pinned snapshot verbatim, so it
is structurally immune to every match-kind change above.

- **30/30 curated exact records resolve byte-identical** through the reshaped resolver
  (all six axes), verified against the prior generated TS interpreter as oracle.
- Case-insensitive path identical (`DATABRICKS-GPT-5-4-NANO` == `databricks-gpt-5-4-nano`).
- Every delta enumerated below is an **uncurated or adversarial** input. Zero curated regressions.

One curated record (`databricks-gpt-5-2`) carries a snapshot that **deliberately diverges**
from what the kept `gpt-5` prefix would now produce — it pins the removed
`dbv2-gpt-code-names-segment` outcome (`efforts none..xhigh + openai-clamp-max-to-xhigh`
vs the prefix's `minimal..high + openai-standard`). The divergence is the reason it is
pinned rather than derived, and is noted in its `_provenance`.

## Complete delta enumeration

Every delta falls into one of eight mechanism classes. Each is a change to an
**uncurated** input; the "Reshaped result" column is the new resolver's output.

| Class | Trigger grammar | Prior result | Reshaped result | Kind of change |
|---|---|---|---|---|
| **B1** | `openai` `gpt-5-<1–3 digit>` (`gpt-5-10`, `gpt-5-2`, `gpt-5-9.1`, `gpt-5-2-mini`) | provider fallback (`none..xhigh`, clamp) | base GPT-5 prefix (`minimal..high`, `openai-standard`) | overmatch: base guard lost |
| **B2** | `databricks_v2` `gpt-5-<1–3 digit>` | `dbv2-gpt-code-names-segment` (`none..xhigh`, clamp) | base GPT-5 prefix (`minimal..high`, `openai-standard`) | effort/normalization narrows; route unchanged (`openai-responses`) |
| **C** | `openai`/`databricks*` token **embedded** mid-string (`gpt-4-gpt-5-pro`) | `openai-gpt5-pro` | provider fallback | embedded-token capture lost |
| **D** | `openai` `.`-suffixed token (`gpt-5.6.x`, `gpt-5-pro.x`) | mixed (base or fallback) | prefix binds on `.` boundary | boundary widened to include `.` |
| **E1** | `databricks_v2` `gpt-<non-5 version>` (`gpt-6`, `gpt-4o`) | `openai-responses` | `mlflow-chat` (DBv2 concrete-unknown) | gpt-version-segment routing lost |
| **E2** | `databricks_v2` dashless `gpt5`-segment (`databricks-gpt5-custom`, `databricks-gpt--5`) | `openai-responses` | `mlflow-chat` | gpt-version-segment routing lost |
| **F** | `databricks_v2` bare Claude code-name segment, no leading `claude` (`opus-5`, `goose-opus-5`, `gpt-opus-5`) | `anthropic-messages` (`omit-fields`) | `mlflow-chat` | segment-anywhere Claude routing lost |
| **G** | `databricks_v2` bare `sol`/`luna`/`terra` segment | `openai-responses` | `mlflow-chat` | sol/luna/terra segment routing lost |
| **H** | `anthropic`/`databricks_v2` kept-prefix token followed by a digit/letter with no separator (`claude-35`, `claude-opus-4-70`) | prior `startsWith` prefix bound | provider fallback | boundary-aware prefix narrows an over-broad `startsWith` |

### Route-impact summary

Only classes **E1/E2**, **F**, and **G** change the `databricks_v2_wire_route`, and every
one moves to `mlflow-chat` — Databricks's universal OpenAI-compatible wire. For dropped
Claude routes (class **F**) this is a **soft degradation**, not a failure: `mlflow-chat`
still serves the model; only the `anthropic-messages` prompt-cache optimization is lost.
Classes **B/C/D/H** change effort sets and/or normalization but never the wire route.

## Normative corpus deltas (8)

The committed corpus pins these eight intentional deltas (all uncurated/adversarial), each
with the prior→reshaped values:

| Corpus id | Input | Class | Changed axes |
|---|---|---|---|
| `openai-multi-digit-version-gpt5-10` | `openai` `gpt-5-10` | B1 | supported_efforts, normalization_policy |
| `openai-gpt5-10-preview-reject-base` | `openai` `gpt-5-10-preview` | B1 | supported_efforts, normalization_policy |
| `openai-gpt5-2-mini-reject-base` | `openai` `gpt-5-2-mini` | B1 | supported_efforts, normalization_policy |
| `openai-gpt5-9-dot-1-reject-base` | `openai` `gpt-5-9.1` | B1 | supported_efforts, normalization_policy |
| `dbv2-gpt5-segment-positive` | `databricks_v2` `databricks-gpt5-custom` | E2 | supported_efforts, wire_route, normalization_policy |
| `dbv2-gpt-doubled-separator` | `databricks_v2` `databricks-gpt--5` | E2 | wire_route |
| `dbv2-goose-opus-5-is-anthropic` | `databricks_v2` `goose-opus-5` | F | thinking_mode, supported_efforts, default_effort, wire_route, normalization_policy |
| `dbv2-dual-marker-gpt-wins-openai` | `databricks_v2` `gpt-opus-5` | F | thinking_mode, supported_efforts, default_effort, wire_route, normalization_policy |
