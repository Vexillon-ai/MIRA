// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/error.rs

use thiserror::Error;

/// Every failure mode of the sandbox that a caller might want to distinguish.
/// A *non-zero exit* is not an error — callers read `SandboxOutput.exit_code`
/// and decide what to do. Only things that prevent us from producing a valid
/// `SandboxOutput` surface as `SandboxError`.
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox backend is not supported on this platform")]
    Unsupported,

    #[error("sandbox child exceeded wall-clock timeout of {0} ms")]
    Timeout(u64),

    #[error("sandbox child output exceeded cap of {0} bytes")]
    OutputTooLarge(usize),

    #[error("sandbox failed to spawn child: {0}")]
    SpawnFailed(String),

    #[error("sandbox policy violation: {0}")]
    Policy(String),

    #[error("sandbox I/O error: {0}")]
    Io(#[from] std::io::Error),
}
