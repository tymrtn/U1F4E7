// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Embed the built frontend into the binary at compile time.
//!
//! This lets `cargo install` produce a single binary with no runtime file
//! dependencies — the dashboard ships inside the executable.

use rust_embed::RustEmbed;

/// The Envelope v2 webmail SPA, compiled from `web/` by SvelteKit +
/// adapter-static and committed under `web/build/`. Embedded so `cargo install`
/// never needs Node — the built bundle ships inside the binary and is served at
/// the site root (see `lib.rs`). As of 1.0.0 this is the only dashboard; the v1
/// `static/` bundle was removed at the v2 cutover.
#[derive(RustEmbed)]
#[folder = "web/build/"]
pub struct WebAssets;

impl WebAssets {
    pub fn get_file(path: &str) -> Option<Vec<u8>> {
        Self::get(path).map(|f| f.data.into_owned())
    }
}
