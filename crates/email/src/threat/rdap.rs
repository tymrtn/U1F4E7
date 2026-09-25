// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! RDAP abuse contact for `threat report`.
//!
//! IANA's DNS bootstrap file names the registry RDAP server for a TLD; the
//! registry's domain answer usually embeds the registrar entity with its
//! `abuse` contact, and otherwise links (`rel: related`) to the registrar's
//! own RDAP answer, which is asked next. Only the domain name is sent. Every
//! query about a domain is returned as a `lookup_performed` record; fetching
//! the bootstrap file discloses nothing and is not recorded.
//!
//! All HTTP goes through [`crate::http::client_for`] with
//! [`Allowance::Public`]: no private addresses, no redirects, 15 s total.

use serde_json::Value;

use super::LookupRecord;
use crate::http::{Allowance, client_for};

pub const PROVIDER: &str = "rdap";
pub const IANA_DNS_BOOTSTRAP: &str = "https://data.iana.org/rdap/dns.json";
/// Largest RDAP document read (the bootstrap file is ~100 KB).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// The registry RDAP base URL for `domain`: the longest TLD suffix listed in
/// the bootstrap `services`, preferring an https URL.
pub fn base_url_for(bootstrap: &Value, domain: &str) -> Option<String> {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let mut best: Option<(usize, String)> = None;
    for service in bootstrap.get("services")?.as_array()? {
        let (Some(tlds), Some(urls)) = (
            service.get(0).and_then(Value::as_array),
            service.get(1).and_then(Value::as_array),
        ) else {
            continue;
        };
        let urls: Vec<&str> = urls.iter().filter_map(Value::as_str).collect();
        let Some(url) = urls
            .iter()
            .find(|u| u.starts_with("https://"))
            .or_else(|| urls.first())
        else {
            continue;
        };
        for tld in tlds.iter().filter_map(Value::as_str) {
            let tld = tld.to_ascii_lowercase();
            let matches = domain == tld || domain.ends_with(&format!(".{tld}"));
            if matches && best.as_ref().is_none_or(|(len, _)| tld.len() > *len) {
                best = Some((tld.len(), url.to_string()));
            }
        }
    }
    best.map(|(_, url)| url)
}

/// `<base>domain/<domain>`, tolerating a base without a trailing slash.
pub fn domain_url(base: &str, domain: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}/domain/{domain}")
}

fn vcard_email(entity: &Value) -> Option<String> {
    entity
        .get("vcardArray")?
        .get(1)?
        .as_array()?
        .iter()
        .filter_map(Value::as_array)
        .find(|prop| prop.first().and_then(Value::as_str) == Some("email"))
        .and_then(|prop| prop.get(3))
        .and_then(Value::as_str)
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| {
            let at = e.find('@');
            at.is_some_and(|i| i > 0 && i < e.len() - 1) && !e.contains(char::is_whitespace)
        })
}

/// The email of the first entity with role `abuse`, searched depth-first
/// through nested `entities`.
pub fn abuse_email(rdap: &Value) -> Option<String> {
    let entities = rdap.get("entities")?.as_array()?;
    for entity in entities {
        let is_abuse = entity
            .get("roles")
            .and_then(Value::as_array)
            .is_some_and(|roles| roles.iter().any(|r| r.as_str() == Some("abuse")));
        if is_abuse && let Some(email) = vcard_email(entity) {
            return Some(email);
        }
        if let Some(found) = abuse_email(entity) {
            return Some(found);
        }
    }
    None
}

/// `links` with `rel: related` pointing at another RDAP JSON document over
/// https (the registrar's answer on thin registries).
pub fn related_links(rdap: &Value) -> Vec<String> {
    rdap.get("links")
        .and_then(Value::as_array)
        .map(|links| {
            links
                .iter()
                .filter(|l| l.get("rel").and_then(Value::as_str) == Some("related"))
                .filter(|l| {
                    l.get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|t| t == "application/rdap+json")
                })
                .filter_map(|l| l.get("href").and_then(Value::as_str))
                .filter(|href| href.starts_with("https://"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The HTTP seam, so tests serve fixtures.
#[allow(async_fn_in_trait)]
pub trait RdapFetch {
    async fn get_json(&self, url: &str) -> Result<Value, String>;
}

/// Public-internet RDAP over the guarded client.
pub struct PublicRdap;

impl RdapFetch for PublicRdap {
    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let (client, url) = client_for(url, &Allowance::Public)
            .await
            .map_err(|e| e.to_string())?;
        let host = url.host_str().unwrap_or_default().to_string();
        let mut resp = client
            .get(url)
            .header("Accept", "application/rdap+json, application/json")
            .send()
            .await
            .map_err(|e| format!("{host}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{host} answered HTTP {}", resp.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{host}: {e}"))? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(format!("{host} answer exceeds {MAX_BODY_BYTES} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|e| format!("{host} answer is not JSON: {e}"))
    }
}

/// Where the abuse contact search ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbuseLookup {
    pub domain: String,
    pub result: Result<String, String>,
    /// One record per RDAP query that named the domain.
    pub lookups: Vec<LookupRecord>,
}

/// Find the abuse email for `domain`: bootstrap, registry, then at most one
/// registrar hop.
pub async fn abuse_contact<F: RdapFetch>(fetch: &F, domain: &str) -> AbuseLookup {
    let mut lookups = Vec::new();
    let result = search(fetch, domain, &mut lookups).await;
    AbuseLookup {
        domain: domain.to_string(),
        result,
        lookups,
    }
}

async fn search<F: RdapFetch>(
    fetch: &F,
    domain: &str,
    lookups: &mut Vec<LookupRecord>,
) -> Result<String, String> {
    let bootstrap = fetch
        .get_json(IANA_DNS_BOOTSTRAP)
        .await
        .map_err(|e| format!("IANA RDAP bootstrap: {e}"))?;
    let base = base_url_for(&bootstrap, domain)
        .ok_or_else(|| format!("no RDAP server is registered for {domain}'s TLD"))?;

    let registry = fetch.get_json(&domain_url(&base, domain)).await;
    let registry = match registry {
        Ok(doc) => doc,
        Err(e) => {
            lookups.push(LookupRecord::new(PROVIDER, domain, format!("error: {e}")));
            return Err(format!("registry RDAP: {e}"));
        }
    };
    if let Some(email) = abuse_email(&registry) {
        lookups.push(LookupRecord::new(PROVIDER, domain, "abuse_contact_found"));
        return Ok(email);
    }
    lookups.push(LookupRecord::new(PROVIDER, domain, "no_abuse_contact"));

    let Some(registrar_url) = related_links(&registry).into_iter().next() else {
        return Err("registry RDAP names no abuse contact and links no registrar".into());
    };
    match fetch.get_json(&registrar_url).await {
        Ok(doc) => match abuse_email(&doc) {
            Some(email) => {
                lookups.push(LookupRecord::new(PROVIDER, domain, "abuse_contact_found"));
                Ok(email)
            }
            None => {
                lookups.push(LookupRecord::new(PROVIDER, domain, "no_abuse_contact"));
                Err("neither registry nor registrar RDAP names an abuse contact".into())
            }
        },
        Err(e) => {
            lookups.push(LookupRecord::new(PROVIDER, domain, format!("error: {e}")));
            Err(format!("registrar RDAP: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Trimmed from IANA's live `dns.json` (2026-09-25).
    fn bootstrap() -> Value {
        json!({
            "version": "1.0",
            "services": [
                [["com"], ["https://rdap.verisign.com/com/v1/"]],
                [["org", "ngo"], ["https://rdap.publicinterestregistry.org/rdap/"]],
                [["uk"], ["http://rdap.nominet.uk/uk/", "https://rdap.nominet.uk/uk/"]],
                [["co.example"], ["https://rdap.second-level.example/"]]
            ]
        })
    }

    /// Trimmed from Verisign's live answer for paypal.com (2026-09-25).
    fn verisign_paypal() -> Value {
        json!({
            "objectClassName": "domain",
            "ldhName": "PAYPAL.COM",
            "links": [
                {"rel": "self", "type": "application/rdap+json",
                 "href": "https://rdap.verisign.com/com/v1/domain/paypal.com"},
                {"rel": "related", "type": "application/rdap+json",
                 "href": "https://rdap.markmonitor.com/rdap/domain/PAYPAL.COM"}
            ],
            "entities": [{
                "objectClassName": "entity",
                "roles": ["registrar"],
                "vcardArray": ["vcard", [["version", {}, "text", "4.0"],
                                         ["fn", {}, "text", "MarkMonitor Inc."]]],
                "entities": [{
                    "objectClassName": "entity",
                    "roles": ["abuse"],
                    "vcardArray": ["vcard", [["version", {}, "text", "4.0"],
                                             ["fn", {}, "text", ""],
                                             ["tel", {"type": "voice"}, "uri", "tel:+1.2086851750"],
                                             ["email", {}, "text", "abusecomplaints@markmonitor.com"]]]
                }]
            }]
        })
    }

    #[test]
    fn bootstrap_picks_the_longest_tld_and_prefers_https() {
        let b = bootstrap();
        assert_eq!(
            base_url_for(&b, "paypal.com").as_deref(),
            Some("https://rdap.verisign.com/com/v1/")
        );
        assert_eq!(
            base_url_for(&b, "Example.ORG.").as_deref(),
            Some("https://rdap.publicinterestregistry.org/rdap/")
        );
        assert_eq!(
            base_url_for(&b, "bank.co.uk").as_deref(),
            Some("https://rdap.nominet.uk/uk/")
        );
        assert_eq!(
            base_url_for(&b, "brand.co.example").as_deref(),
            Some("https://rdap.second-level.example/")
        );
        assert_eq!(base_url_for(&b, "nothing.invalid"), None);
        assert_eq!(
            domain_url("https://rdap.verisign.com/com/v1/", "paypal.com"),
            "https://rdap.verisign.com/com/v1/domain/paypal.com"
        );
    }

    #[test]
    fn abuse_email_is_found_in_the_nested_registrar_entity() {
        assert_eq!(
            abuse_email(&verisign_paypal()).as_deref(),
            Some("abusecomplaints@markmonitor.com")
        );
        assert_eq!(abuse_email(&json!({"entities": []})), None);
        assert_eq!(
            related_links(&verisign_paypal()),
            vec!["https://rdap.markmonitor.com/rdap/domain/PAYPAL.COM".to_string()]
        );
    }

    struct Fixtures(
        HashMap<&'static str, Result<Value, String>>,
        RefCell<Vec<String>>,
    );

    impl RdapFetch for Fixtures {
        async fn get_json(&self, url: &str) -> Result<Value, String> {
            self.1.borrow_mut().push(url.to_string());
            self.0
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("unexpected fetch {url}")))
        }
    }

    #[tokio::test]
    async fn registry_answer_with_abuse_contact_needs_no_second_hop() {
        let f = Fixtures(
            HashMap::from([
                (IANA_DNS_BOOTSTRAP, Ok(bootstrap())),
                (
                    "https://rdap.verisign.com/com/v1/domain/paypal.com",
                    Ok(verisign_paypal()),
                ),
            ]),
            RefCell::default(),
        );
        let found = abuse_contact(&f, "paypal.com").await;
        assert_eq!(
            found.result.as_deref(),
            Ok("abusecomplaints@markmonitor.com")
        );
        assert_eq!(
            found.lookups,
            vec![LookupRecord::new(
                PROVIDER,
                "paypal.com",
                "abuse_contact_found"
            )]
        );
        assert_eq!(f.1.borrow().len(), 2);
    }

    #[tokio::test]
    async fn thin_registry_answer_follows_the_registrar_link() {
        let mut registry = verisign_paypal();
        registry["entities"][0]["entities"] = json!([]);
        let registrar = json!({"entities": [{"roles": ["abuse"], "vcardArray":
            ["vcard", [["email", {}, "text", "Abuse@Registrar.Example"]]]}]});
        let f = Fixtures(
            HashMap::from([
                (IANA_DNS_BOOTSTRAP, Ok(bootstrap())),
                (
                    "https://rdap.verisign.com/com/v1/domain/paypal.com",
                    Ok(registry),
                ),
                (
                    "https://rdap.markmonitor.com/rdap/domain/PAYPAL.COM",
                    Ok(registrar),
                ),
            ]),
            RefCell::default(),
        );
        let found = abuse_contact(&f, "paypal.com").await;
        assert_eq!(found.result.as_deref(), Ok("abuse@registrar.example"));
        assert_eq!(found.lookups.len(), 2);
        assert!(found.lookups.iter().all(|l| l.domain == "paypal.com"));
    }

    #[tokio::test]
    async fn failures_are_reported_not_swallowed() {
        let f = Fixtures(
            HashMap::from([(IANA_DNS_BOOTSTRAP, Err("timed out".to_string()))]),
            RefCell::default(),
        );
        let found = abuse_contact(&f, "paypal.com").await;
        assert!(found.result.unwrap_err().contains("bootstrap"));
        assert!(
            found.lookups.is_empty(),
            "the bootstrap discloses no domain"
        );

        let f = Fixtures(
            HashMap::from([
                (IANA_DNS_BOOTSTRAP, Ok(bootstrap())),
                (
                    "https://rdap.verisign.com/com/v1/domain/paypal.com",
                    Err("HTTP 503".to_string()),
                ),
            ]),
            RefCell::default(),
        );
        let found = abuse_contact(&f, "paypal.com").await;
        assert!(found.result.unwrap_err().contains("503"));
        assert_eq!(found.lookups[0].result, "error: HTTP 503");
    }
}
