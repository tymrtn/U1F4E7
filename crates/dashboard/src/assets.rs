// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Embed the `static/` directory into the binary at compile time.
//!
//! This lets `cargo install envelope-email` produce a single binary with
//! no runtime file dependencies — the dashboard HTML/CSS/JS ships inside
//! the executable.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct Assets;

impl Assets {
    pub fn get_file(path: &str) -> Option<Vec<u8>> {
        Self::get(path).map(|f| f.data.into_owned())
    }
}
