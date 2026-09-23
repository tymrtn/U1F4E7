// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Domain helpers shared by the analyzers: registrable domain, punycode, and
//! look-alike comparison.

/// Two-label public suffixes common enough to matter for mail. Not the full
/// Public Suffix List; an unknown multi-label suffix degrades to "last two
/// labels", which only ever makes look-alike checks stricter.
const TWO_LABEL_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "ltd.uk", "plc.uk", "com.au", "net.au", "org.au",
    "co.nz", "co.jp", "ne.jp", "co.za", "com.br", "com.mx", "com.ar", "com.tr", "com.cn", "com.hk",
    "com.sg", "co.in", "co.kr", "com.es",
];

/// Lowercased domain part of an address.
pub fn domain_of(addr: &str) -> Option<String> {
    let (_, domain) = addr.trim().rsplit_once('@')?;
    let domain = domain
        .trim()
        .trim_end_matches('>')
        .trim_end_matches('.')
        .to_lowercase();
    (!domain.is_empty()).then_some(domain)
}

/// eTLD+1 of `host` (see [`TWO_LABEL_SUFFIXES`]).
pub fn registrable(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    let last_two = labels[labels.len() - 2..].join(".");
    let keep = if TWO_LABEL_SUFFIXES.contains(&last_two.as_str()) {
        3
    } else {
        2
    };
    labels[labels.len().saturating_sub(keep)..].join(".")
}

pub fn has_punycode(host: &str) -> bool {
    host.split('.')
        .any(|l| l.to_ascii_lowercase().starts_with("xn--"))
}

/// Unicode form of a (possibly punycode) host; the input on failure.
pub fn to_unicode(host: &str) -> String {
    let (unicode, result) = idna::domain_to_unicode(host);
    if result.is_ok() {
        unicode
    } else {
        host.to_string()
    }
}

/// Collapse visually confusable characters so `pаypal` (Cyrillic а),
/// `paypa1` and `rnicrosoft` compare equal to their targets.
pub fn skeleton(host: &str) -> String {
    let unicode = to_unicode(host).to_lowercase();
    let mapped: String = unicode
        .chars()
        .map(|c| match c {
            'а' | 'ɑ' | 'α' => 'a',
            'с' | 'ϲ' => 'c',
            'ԁ' => 'd',
            'е' | 'ё' | 'ε' => 'e',
            'ɡ' => 'g',
            'һ' => 'h',
            'і' | 'ı' | 'ι' => 'i',
            '1' | '|' | 'ӏ' => 'l',
            'ј' => 'j',
            'к' | 'κ' => 'k',
            'м' => 'm',
            'о' | 'ο' | '0' => 'o',
            'р' | 'ρ' => 'p',
            'ԛ' => 'q',
            'ѕ' => 's',
            'т' | 'τ' => 't',
            'υ' | 'ս' => 'u',
            'ѵ' | 'ν' => 'v',
            'ԝ' | 'ω' => 'w',
            'х' | 'χ' => 'x',
            'у' => 'y',
            other => other,
        })
        .collect();
    mapped.replace("rn", "m").replace("vv", "w")
}

/// Levenshtein distance, bounded to short strings (domains).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The known domain `host` imitates, if any: same skeleton or one edit away,
/// but not the same registrable domain. Labels under five characters are
/// too short for edit distance to mean anything.
pub fn lookalike_of<'a>(host: &str, known: &'a [String]) -> Option<&'a str> {
    let candidate = registrable(host);
    if candidate.is_empty() {
        return None;
    }
    let candidate_skeleton = skeleton(&candidate);
    known.iter().map(String::as_str).find(|k| {
        let k_reg = registrable(k);
        if k_reg == candidate || k_reg.is_empty() {
            return false;
        }
        if skeleton(&k_reg) == candidate_skeleton {
            return true;
        }
        let long_enough = k_reg
            .split('.')
            .next()
            .is_some_and(|l| l.chars().count() >= 5);
        long_enough && edit_distance(&k_reg, &candidate) == 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn registrable_handles_two_label_suffixes() {
        assert_eq!(registrable("mx1.mail.example.co.uk"), "example.co.uk");
        assert_eq!(registrable("login.paypal.com"), "paypal.com");
        assert_eq!(registrable("Example.COM."), "example.com");
    }

    #[test]
    fn lookalikes_by_edit_distance_and_homoglyph() {
        let k = known(&["paypal.com", "example.org"]);
        assert_eq!(lookalike_of("paypa1.com", &k), Some("paypal.com"));
        assert_eq!(lookalike_of("secure.paypall.com", &k), Some("paypal.com"));
        assert_eq!(lookalike_of("examp1e.org", &k), Some("example.org"));
        // Cyrillic 'а' in punycode.
        let spoof = idna::domain_to_ascii("p\u{0430}ypal.com").unwrap();
        assert!(spoof.starts_with("xn--"));
        assert_eq!(lookalike_of(&spoof, &k), Some("paypal.com"));
        // The real thing and unrelated domains are not look-alikes.
        assert_eq!(lookalike_of("www.paypal.com", &k), None);
        assert_eq!(lookalike_of("github.com", &k), None);
    }

    #[test]
    fn short_labels_do_not_trip_edit_distance() {
        let k = known(&["abc.com"]);
        assert_eq!(lookalike_of("abd.com", &k), None);
    }

    #[test]
    fn punycode_is_detected_and_decoded() {
        let spoof = idna::domain_to_ascii("p\u{0430}ypal.com").unwrap();
        assert!(has_punycode(&spoof));
        assert_eq!(to_unicode(&spoof), "p\u{0430}ypal.com");
        assert!(!has_punycode("paypal.com"));
    }
}
