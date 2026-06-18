// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/output.rs

/// What a successful sandbox run produces. "Successful" here means the
/// parent got to observe the child finishing — the child may still have
/// exited non-zero, which is the caller's business to interpret.
#[derive(Debug, Clone)]
pub struct SandboxOutput {
    pub stdout:      String,
    pub stderr:      String,
    pub exit_code:   i32,
    pub duration_ms: u64,
    /// True when output exceeded `ResourceLimits::max_output_bytes` and the
    /// parent truncated the captured streams. The child was still allowed
    /// to finish writing — the extra bytes were just discarded.
    pub truncated:   bool,
}
