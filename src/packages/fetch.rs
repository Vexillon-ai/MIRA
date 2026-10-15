// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fetch a package bundle by URL (the install-from-URL
//! primitive).
//!
//! Install-by-URL is the base operation the whole distribution feature builds
//! on: "browse & install" just resolves an index entry to a `download_url`
//! and calls this, and pasting a raw `.mirapkg`/`.miraskill` URL works too. The
//! fetched bytes then flow through the **existing** parse → verify → trust-gate →
//! install path unchanged — the URL is only a transport for the same bytes an
//! admin would otherwise upload.
//!
//! Egress discipline: the fetch goes through the instance
//! [`HttpPolicy`] so it inherits the SSRF guard, denylist/allowlist, proxy,
//! redirect-pinning and Restricted-Mode gate — and it is always admin-initiated
//! (the only callers are the admin-gated HTTP handlers). We fail closed on a
//! non-2xx status or a truncated/oversize body: a partial binary must never
//! reach the installer.

use crate::tools::http_policy::HttpPolicy;

/// Hard ceiling on a fetched bundle — mirrors the upload path's
/// `MAX_BUNDLE_BYTES` (10 MB compressed). The caller builds its `HttpPolicy`
/// with `max_body_bytes` set to at least this so a legitimate package isn't
/// truncated by the default (text-oriented) web-fetch cap.
pub const MAX_FETCH_BYTES: u64 = 10 * 1024 * 1024;

/// Fetch a `.mirapkg`/`.miraskill` by URL through `policy`, as an admin-initiated
/// egress. Returns the raw bundle bytes, or a human-readable error suitable for a
/// 400 response. Never returns a partial body.
pub async fn fetch_bundle(policy: &HttpPolicy, url: &str, admin_id: &str) -> Result<Vec<u8>, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("no package URL given".into());
    }
    let resp = policy
        .get(url, admin_id)
        .await
        .map_err(|e| format!("could not fetch {url}: {e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("fetching {url} returned HTTP {}", resp.status));
    }
    if resp.truncated {
        return Err(format!(
            "the package at {url} is larger than the fetch limit ({} bytes) — download it and upload the file directly",
            MAX_FETCH_BYTES,
        ));
    }
    if resp.body.is_empty() {
        return Err(format!("fetching {url} returned an empty body"));
    }
    Ok(resp.body)
}
