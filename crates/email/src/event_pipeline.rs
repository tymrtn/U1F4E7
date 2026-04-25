// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::code_extractor::{
    OtpPatternId, extract_code_with_pattern, parse_expiry_hint, redact_codes,
};
use crate::event_types::{
    ClassifiedEvent, EmailEvent, NewMessagePayload, OtpDetectedPayload, OtpSecret,
};
use envelope_email_store::models::Message;

pub struct OtpClassifier {
    min_confidence: f32,
}

impl Default for OtpClassifier {
    fn default() -> Self {
        Self {
            min_confidence: 0.5,
        }
    }
}

impl OtpClassifier {
    pub fn new(min_confidence: f32) -> Self {
        Self { min_confidence }
    }

    pub fn classify(&self, message: &Message, folder: &str) -> Option<ClassifiedEvent> {
        let text = message.text_body.as_deref().unwrap_or_default();
        let html = message.html_body.as_deref();
        let (code, source_pattern) = extract_code_with_pattern(text, html)?;
        let confidence = confidence_for_pattern(source_pattern);
        if confidence < self.min_confidence {
            return None;
        }

        Some(ClassifiedEvent {
            event: EmailEvent::OtpDetected(OtpDetectedPayload {
                uid: message.uid,
                folder: folder.to_string(),
                from_addr: Some(message.from_addr.clone()),
                subject_redacted: redact_text(&message.subject),
                code_length: code.len(),
                confidence,
                source_pattern,
                expires_hint_secs: parse_expiry_hint(text),
            }),
            secret: Some(OtpSecret { code }),
        })
    }
}

#[derive(Default)]
pub struct NewMessageClassifier;

impl NewMessageClassifier {
    pub fn classify(&self, message: &Message, folder: &str) -> ClassifiedEvent {
        ClassifiedEvent {
            event: EmailEvent::NewMessage(NewMessagePayload {
                uid: message.uid,
                folder: folder.to_string(),
                message_id: message.message_id.clone(),
                from_addr: message.from_addr.clone(),
                subject_redacted: redact_text(&message.subject),
                snippet_redacted: message
                    .text_body
                    .as_deref()
                    .map(redact_text)
                    .map(|text| truncate(&text, 160)),
            }),
            secret: None,
        }
    }
}

#[derive(Default)]
pub struct EventPipeline {
    otp: OtpClassifier,
    new_message: NewMessageClassifier,
}

impl EventPipeline {
    pub fn classify(&self, message: &Message, folder: &str) -> Vec<ClassifiedEvent> {
        let mut events = vec![self.new_message.classify(message, folder)];
        if let Some(otp) = self.otp.classify(message, folder) {
            events.push(otp);
        }
        events
    }
}

fn confidence_for_pattern(pattern: OtpPatternId) -> f32 {
    match pattern {
        OtpPatternId::ExplicitLabel => 0.95,
        OtpPatternId::OtpStyle => 0.9,
        OtpPatternId::HtmlProminent => 0.7,
        OtpPatternId::Fallback => 0.4,
    }
}

fn redact_text(input: &str) -> String {
    redact_codes(input)
}

fn truncate(input: &str, limit: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(subject: &str, text_body: Option<&str>, html_body: Option<&str>) -> Message {
        Message {
            uid: 42,
            message_id: Some("<msg@example.com>".to_string()),
            from_addr: "noreply@example.com".to_string(),
            to_addr: "user@example.com".to_string(),
            cc_addr: None,
            subject: subject.to_string(),
            date: None,
            text_body: text_body.map(str::to_string),
            html_body: html_body.map(str::to_string),
            in_reply_to: None,
            references: None,
            flags: vec![],
            attachments: vec![],
        }
    }

    #[test]
    fn otp_classifier_reports_pattern_and_expiry() {
        let classifier = OtpClassifier::default();
        let event = classifier
            .classify(
                &message(
                    "Your verification code is 123456",
                    Some("Your verification code is 123456. Expires in 10 minutes."),
                    None,
                ),
                "INBOX",
            )
            .unwrap();

        match event.event {
            EmailEvent::OtpDetected(payload) => {
                assert_eq!(payload.source_pattern, OtpPatternId::ExplicitLabel);
                assert_eq!(payload.confidence, 0.95);
                assert_eq!(payload.expires_hint_secs, Some(600));
                assert_eq!(payload.subject_redacted, "Your verification code is ***");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn fallback_pattern_is_below_default_cutline() {
        let classifier = OtpClassifier::default();
        assert!(
            classifier
                .classify(&message("Hello", Some("123456"), None), "INBOX")
                .is_none()
        );
    }

    #[test]
    fn fallback_pattern_can_be_enabled_with_lower_cutline() {
        let classifier = OtpClassifier::new(0.4);
        let event = classifier
            .classify(&message("Hello", Some("123456"), None), "INBOX")
            .unwrap();

        match event.event {
            EmailEvent::OtpDetected(payload) => {
                assert_eq!(payload.source_pattern, OtpPatternId::Fallback);
                assert_eq!(payload.confidence, 0.4);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn new_message_classifier_redacts_body_and_subject() {
        let classifier = NewMessageClassifier;
        let event = classifier.classify(
            &message("Subject 123456", Some("Use 123456 to sign in"), None),
            "INBOX",
        );

        match event.event {
            EmailEvent::NewMessage(payload) => {
                assert_eq!(payload.subject_redacted, "Subject ***");
                assert_eq!(
                    payload.snippet_redacted.as_deref(),
                    Some("Use *** to sign in")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn redaction_invariant_hides_secret_from_debug_and_serialization() {
        let classifier = OtpClassifier::default();
        let event = classifier
            .classify(
                &message(
                    "Your code is 482910",
                    Some("Your verification code is 482910"),
                    None,
                ),
                "INBOX",
            )
            .unwrap();

        let debug = format!("{event:?}");
        assert!(!debug.contains("482910"));
        assert!(debug.contains("<redacted>"));

        let json = serde_json::to_string(&event.event).unwrap();
        assert!(!json.contains("482910"));
        assert!(json.contains("\"code_length\":6"));
    }

    #[test]
    fn event_pipeline_emits_new_message_and_otp() {
        let pipeline = EventPipeline::default();
        let events = pipeline.classify(
            &message(
                "Your verification code is 123456",
                Some("Your verification code is 123456"),
                None,
            ),
            "INBOX",
        );

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event, EmailEvent::NewMessage(_)));
        assert!(matches!(events[1].event, EmailEvent::OtpDetected(_)));
    }
}
