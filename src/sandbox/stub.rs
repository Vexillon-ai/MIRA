// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/stub.rs
//! Fallback backend for non-Linux targets and for builds with the
//! `sandbox-linux` feature disabled. Always returns
//! `SandboxError::Unsupported`.

use async_trait::async_trait;

use super::{CodeSandbox, Language, ResourceLimits, SandboxOutput, SandboxError};

pub struct UnsupportedSandbox;

#[async_trait]
impl CodeSandbox for UnsupportedSandbox {
    async fn run(
        &self,
        _language: Language,
        _payload:  &str,
        _stdin:    Option<&str>,
        _limits:   &ResourceLimits,
    ) -> Result<SandboxOutput, SandboxError> {
        Err(SandboxError::Unsupported)
    }

    fn name(&self) -> &'static str { "unsupported" }
    fn supported(&self) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn always_returns_unsupported() {
        let s = UnsupportedSandbox;
        let r = s.run(Language::Bash, "echo hi", None, &ResourceLimits::default()).await;
        assert!(matches!(r, Err(SandboxError::Unsupported)));
        assert!(!s.supported());
    }
}
