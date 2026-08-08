// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Vendored, weight-free projection of the Governor **envelope** catalog.
//!
//! Governor is the protected scoring engine; its weights and thresholds are
//! never embedded in Envelope. This module vendors only the *public* projection
//! — key, description, category, catalog version — from a checked-in JSON file
//! (`governor_catalog.gen.json`) and enriches it with Envelope's own provenance
//! policy for discovery. There are no weights, thresholds, or scores anywhere in
//! this module, and the drift test guards the vendored file against the live
//! Governor projection when a binary is available.
//!
//! Vendoring (rather than calling `governor catalog` per validation) is
//! deliberate: attribution validation must still work when the Governor binary
//! is absent — which is exactly the `governor_unavailable` moment when a bot
//! most needs to understand the catalog.

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::attribution_provenance::{Provenance, provenance_of};

/// Attribution protocol id stamped into every attribution block and discovery
/// payload. Bumped only on a breaking protocol change.
pub const ATTRIBUTION_PROTOCOL: &str = "envelope.attribution.v1";

/// The Governor catalog Envelope declares against.
pub const CATALOG_NAME: &str = "envelope";

/// The four honesty rules shown verbatim in every recovery and discovery
/// payload. They never mention scores, weights, or thresholds.
pub const HONESTY_RULES: &[&str] = &[
    "Declare only facts true of THIS message. Unknown means omit — never guess.",
    "True risk facts (financial/legal/PII/commitments) must be declared; omitting them is dishonest attribution and is audited.",
    "Never declare attestation attributes; the host records human approval itself.",
    "Declarations are checked against host observations; contradictions invalidate the request.",
];

const VENDORED_JSON: &str = include_str!("governor_catalog.gen.json");

/// One public catalog attribute: key, category, description. No weight.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogAttr {
    pub key: String,
    pub category: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
struct VendoredCatalog {
    #[allow(dead_code)]
    catalog: String,
    catalog_version: u32,
    attributes: Vec<CatalogAttr>,
}

fn vendored() -> &'static VendoredCatalog {
    static CATALOG: OnceLock<VendoredCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(VENDORED_JSON)
            .expect("vendored governor_catalog.gen.json must be valid catalog JSON")
    })
}

/// The pinned catalog version Envelope vendored.
pub fn catalog_version() -> u32 {
    vendored().catalog_version
}

/// All public catalog attributes (key/category/description).
pub fn catalog_attributes() -> &'static [CatalogAttr] {
    &vendored().attributes
}

/// All catalog keys, in vendored (catalog) order.
pub fn catalog_keys() -> Vec<String> {
    vendored()
        .attributes
        .iter()
        .map(|a| a.key.clone())
        .collect()
}

/// Whether `key` is a known catalog attribute.
pub fn is_catalog_key(key: &str) -> bool {
    vendored().attributes.iter().any(|a| a.key == key)
}

/// Description for a catalog key, if known.
pub fn description_of(key: &str) -> Option<&'static str> {
    vendored()
        .attributes
        .iter()
        .find(|a| a.key == key)
        .map(|a| a.description.as_str())
}

/// The declarable-only key set (author-context facts the host cannot observe),
/// in catalog order.
pub fn declarable_keys() -> Vec<String> {
    catalog_keys()
        .into_iter()
        .filter(|k| provenance_of(k) == Some(Provenance::Declarable))
        .collect()
}

/// The full set of keys an agent MAY submit in `attributes`, in catalog order:
/// every `Declarable` key plus every `HostDerived` key. Runtime accepts a
/// declared `HostDerived` key only when Envelope independently observes it true
/// (a corroborated declaration counts; a contradiction/unobservable one is
/// rejected), so the schema must ADVERTISE these keys as submittable even though
/// they may be rejected on verification — matching real runtime. Attestation-only
/// keys (`tyler_approved`, `authorized_campaign`) are excluded: they are never
/// bot-declarable. This is the source the MCP `attributes` enum and contract are
/// derived from, so they never drift by hand.
pub fn agent_submittable_keys() -> Vec<String> {
    catalog_keys()
        .into_iter()
        .filter(|k| {
            matches!(
                provenance_of(k),
                Some(Provenance::Declarable) | Some(Provenance::HostDerived)
            )
        })
        .collect()
}

/// Catalog keys within Levenshtein distance `max` of `key`, in catalog order —
/// the `did_you_mean` repair set for an unknown declared key.
pub fn nearest_keys(key: &str, max: usize) -> Vec<String> {
    catalog_keys()
        .into_iter()
        .filter(|k| levenshtein(key, k) <= max)
        .collect()
}

/// The Envelope-side discovery projection (`governor_catalog` tool / `envelope
/// governor catalog --json`): the vendored catalog enriched with provenance and
/// declaration guidance. Never contains a weight, threshold, or score.
pub fn envelope_projection() -> Value {
    let attributes: Vec<Value> = vendored()
        .attributes
        .iter()
        .map(|a| {
            let prov = provenance_of(&a.key)
                .map(|p| p.as_str())
                .unwrap_or("host_derived");
            let mut entry = json!({
                "key": a.key,
                "category": a.category,
                "provenance": prov,
                "description": a.description,
            });
            if let Some(note) = provenance_note(&a.key)
                && let Value::Object(map) = &mut entry
            {
                map.insert("note".to_string(), Value::String(note.to_string()));
            }
            entry
        })
        .collect();

    json!({
        "protocol": ATTRIBUTION_PROTOCOL,
        "catalog": CATALOG_NAME,
        "catalog_version": catalog_version(),
        "source": "vendored governor.catalog.v1 projection",
        "attributes": attributes,
        "declaration": {
            "mcp": "attributes: [\"<key>\", …] on send / reply / send_draft",
            "cli": "--attr <key> (repeatable) on envelope send, envelope draft send, and envelope unsubscribe (the mailto: compliance send)"
        },
        "rules": HONESTY_RULES,
    })
}

/// A short declaration note for the discovery projection, for host-derived and
/// attestation keys where declaration behavior is non-obvious.
fn provenance_note(key: &str) -> Option<&'static str> {
    match provenance_of(key) {
        Some(Provenance::HostDerived) => {
            Some("derived from the message/store; declaring it is accepted only when consistent")
        }
        Some(Provenance::RequiresAttestation) => Some(
            "recorded by human dashboard approval / operator signal only; agent declarations are rejected",
        ),
        _ => None,
    }
}

/// Iterative Levenshtein edit distance (small strings, no allocation beyond one
/// row). Used only for `did_you_mean` repair suggestions.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_catalog_has_thirty_unique_keys_and_no_weights() {
        let attrs = catalog_attributes();
        assert_eq!(attrs.len(), 30, "envelope catalog is 30 keys");

        let mut keys: Vec<&str> = attrs.iter().map(|a| a.key.as_str()).collect();
        keys.sort_unstable();
        let unique = {
            let mut k = keys.clone();
            k.dedup();
            k.len()
        };
        assert_eq!(unique, 30, "catalog keys must be unique");

        // No weight/score-like field may exist in the vendored JSON.
        let raw: Value = serde_json::from_str(VENDORED_JSON).unwrap();
        let text = raw.to_string();
        for banned in ["weight", "score", "threshold"] {
            assert!(
                !text.contains(banned),
                "vendored catalog must not contain `{banned}`"
            );
        }
        assert_eq!(catalog_version(), 1);
    }

    #[test]
    fn declarable_keys_are_the_six_author_context_keys() {
        let mut d = declarable_keys();
        d.sort();
        assert_eq!(
            d,
            vec![
                "commitment_language",
                "financial_content",
                "has_pii",
                "informational",
                "legal_content",
                "uncited_claims",
            ]
        );
    }

    #[test]
    fn agent_submittable_keys_are_declarable_plus_host_derived_without_attestation() {
        let submittable = agent_submittable_keys();
        // Declarable ∪ HostDerived = 6 + 22 = 28; attestation keys excluded.
        assert_eq!(submittable.len(), 28, "6 declarable + 22 host-derived");
        // Every declarable and host-derived key is submittable.
        for k in declarable_keys() {
            assert!(
                submittable.contains(&k),
                "declarable `{k}` must be submittable"
            );
        }
        for k in crate::attribution_provenance::HOST_DERIVED {
            assert!(
                submittable.contains(&k.to_string()),
                "host-derived `{k}` must be submittable"
            );
        }
        // Attestation-only keys are NEVER submittable.
        for k in crate::attribution_provenance::REQUIRES_ATTESTATION {
            assert!(
                !submittable.contains(&k.to_string()),
                "attestation key `{k}` must not be submittable"
            );
        }
    }

    #[test]
    fn nearest_keys_finds_typo_repairs() {
        assert_eq!(nearest_keys("informationl", 2), vec!["informational"]);
        assert!(nearest_keys("reply_to_thread", 2).contains(&"reply_to_thread".to_string()));
        // A wildly wrong key has no near neighbor.
        assert!(nearest_keys("xyzzyxyzzy", 2).is_empty());
    }

    /// Drift guard: when a Governor build supporting the public catalog
    /// projection command is available, the vendored key set must match it.
    /// Skipped-with-notice otherwise — validation must not require Governor to be
    /// present, and Governor's `catalog` command is a separate (Stage G) change.
    #[test]
    fn vendored_projection_matches_live_governor_when_available() {
        let bin = std::env::var("ENVELOPE_GOVERNOR_BIN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let bin = bin.or_else(|| {
            let default =
                "/Users/tylermartin/Dropbox/Code/governor/governor2/target/release/governor";
            std::path::Path::new(default)
                .exists()
                .then(|| default.to_string())
        });
        let Some(bin) = bin else {
            eprintln!("drift test skipped: no Governor binary available");
            return;
        };
        let output = std::process::Command::new(&bin)
            .args(["catalog", "--catalog", "envelope", "--json"])
            .output();
        let Ok(out) = output else {
            eprintln!("drift test skipped: could not execute {bin}");
            return;
        };
        if !out.status.success() {
            eprintln!("drift test skipped: {bin} does not support `catalog --json` yet");
            return;
        }
        let Ok(live): Result<Value, _> = serde_json::from_slice(&out.stdout) else {
            eprintln!("drift test skipped: live catalog output was not JSON");
            return;
        };
        let Some(live_attrs) = live.get("attributes").and_then(|a| a.as_array()) else {
            eprintln!("drift test skipped: live catalog output had no attributes array");
            return;
        };
        let live_keys: std::collections::BTreeSet<String> = live_attrs
            .iter()
            .filter_map(|a| a.get("key").and_then(|k| k.as_str()).map(String::from))
            .collect();
        let vendored_keys: std::collections::BTreeSet<String> =
            catalog_keys().into_iter().collect();
        assert_eq!(
            vendored_keys, live_keys,
            "vendored catalog drifted from live Governor; run scripts/gen-governor-catalog.sh"
        );
    }

    #[test]
    fn envelope_projection_exposes_provenance_and_no_weights() {
        let proj = envelope_projection();
        assert_eq!(proj["protocol"], "envelope.attribution.v1");
        assert_eq!(proj["catalog_version"], 1);
        let text = proj.to_string();
        for banned in ["weight", "\"score\"", "threshold"] {
            assert!(!text.contains(banned), "projection leaked `{banned}`");
        }
        // tyler_approved must be classified requires_attestation.
        let attrs = proj["attributes"].as_array().unwrap();
        let tyler = attrs.iter().find(|a| a["key"] == "tyler_approved").unwrap();
        assert_eq!(tyler["provenance"], "requires_attestation");
        let fin = attrs
            .iter()
            .find(|a| a["key"] == "financial_content")
            .unwrap();
        assert_eq!(fin["provenance"], "declarable");
    }
}
