// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

use crate::code_extractor::OtpPatternId;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmailEvent {
    NewMessage(NewMessagePayload),
    OtpDetected(OtpDetectedPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMessagePayload {
    pub uid: u32,
    pub folder: String,
    pub message_id: Option<String>,
    pub from_addr: String,
    pub subject_redacted: String,
    pub snippet_redacted: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpDetectedPayload {
    pub uid: u32,
    pub folder: String,
    pub from_addr: Option<String>,
    pub subject_redacted: String,
    pub code_length: usize,
    pub confidence: f32,
    pub source_pattern: OtpPatternId,
    pub expires_hint_secs: Option<u32>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OtpSecret {
    pub code: String,
}

impl fmt::Debug for OtpSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtpSecret")
            .field("code", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub struct ClassifiedEvent {
    pub event: EmailEvent,
    pub secret: Option<OtpSecret>,
}

impl fmt::Debug for ClassifiedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassifiedEvent")
            .field("event", &self.event)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
