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

/// The canonical reader deep link. The folder rides as a query parameter
/// because IMAP UIDs are mailbox-scoped: the same uid names a different message
/// in INBOX than in `[Gmail]/Sent Mail`.
pub(crate) fn message_dashboard_path(
    account_id: &str,
    folder: &str,
    uid: impl std::fmt::Display,
) -> String {
    format!(
        "/mail/unified/{}/{}?folder={}",
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
            "/mail/unified/acc%201/42?folder=Sent%20Items%20%26%20Archive"
        );
    }

    /// Cockpit/rules message links must land on the same canonical reader route
    /// the CLI emits, for the Gmail folder shape that reproduced the 404.
    #[test]
    fn message_dashboard_path_matches_the_canonical_reader_route() {
        assert_eq!(
            message_dashboard_path(
                "109c5747-8498-4614-945a-837462ae0aaf",
                "[Gmail]/Sent Mail",
                33281
            ),
            "/mail/unified/109c5747-8498-4614-945a-837462ae0aaf/33281?folder=%5BGmail%5D%2FSent%20Mail"
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
