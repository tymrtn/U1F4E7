// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Dashboard UI route helpers shared by API handlers.

pub(crate) fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub(crate) fn message_dashboard_path(
    account_id: &str,
    folder: &str,
    uid: impl std::fmt::Display,
) -> String {
    format!(
        "/accounts/{}/messages/{}?folder={}",
        encode_path_segment(account_id),
        uid,
        encode_path_segment(folder)
    )
}

pub(crate) fn draft_dashboard_path(account_id: &str, draft_id: &str) -> String {
    format!(
        "/accounts/{}/drafts/{}",
        encode_path_segment(account_id),
        encode_path_segment(draft_id)
    )
}

#[cfg(test)]
mod tests {
    use super::{draft_dashboard_path, message_dashboard_path};

    #[test]
    fn message_dashboard_path_uses_router_shape_and_percent_encoding() {
        assert_eq!(
            message_dashboard_path("acc 1", "Sent Items & Archive", 42),
            "/accounts/acc%201/messages/42?folder=Sent%20Items%20%26%20Archive"
        );
    }

    #[test]
    fn draft_dashboard_path_percent_encodes_segments() {
        assert_eq!(
            draft_dashboard_path("operator@example.com", "draft/with space"),
            "/accounts/operator%40example.com/drafts/draft%2Fwith%20space"
        );
    }
}
