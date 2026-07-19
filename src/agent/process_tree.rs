// SPDX-License-Identifier: AGPL-3.0-or-later

//! Reap a spawned worker's **whole process tree**, not just the direct child.
//!
//! A coding-agent worker (Claude Code / OpenCode / …) frequently spawns its own
//! children — e.g. the `python -m http.server` the verification step runs. Tokio's
//! `kill_on_drop(true)` only SIGKILLs the *immediate* child, so those grandchildren
//! orphan and linger for hours, littering ports (the 8000/8137/21420 orphans in the
//! 2026-07-19 restart incident). This guard binds the entire tree's lifetime to the
//! worker: create it before spawn, `assign` the child after spawn, and when the
//! guard drops (worker completed or interrupted) every descendant is terminated.
//!
//! - **Windows:** a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; closing
//!   the job handle kills everything in it.
//! - **Unix:** the child leads its own process group; the group is `SIGKILL`ed.
//!
//! Best-effort throughout — a failure to set up reaping is logged, never fatal
//! (`kill_on_drop` still covers the direct child).

use tokio::process::{Child, Command};

pub use imp::ProcessTreeGuard;

// ── Windows: Job Object (KILL_ON_JOB_CLOSE) ──────────────────────────────────
#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Holds the job handle as an `isize` (Send-safe, unlike the raw `HANDLE`
    /// pointer) so the guard can live across await points in the worker future.
    pub struct ProcessTreeGuard {
        job: isize, // 0 = setup failed / no job
    }

    impl ProcessTreeGuard {
        pub fn prepare(_cmd: &mut Command) -> Self {
            // SAFETY: null args = an unnamed job with a fresh handle we own.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                tracing::warn!("process-tree guard: CreateJobObject failed — worker descendants may orphan");
                return Self { job: 0 };
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` outlives the call; class + size match the struct.
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                tracing::warn!("process-tree guard: SetInformationJobObject failed — worker descendants may orphan");
                unsafe { CloseHandle(job) };
                return Self { job: 0 };
            }
            Self { job: job as isize }
        }

        /// Assign the freshly-spawned child (and thus its future descendants) to
        /// the job. Modern Windows nests jobs, so this works even if MIRA itself
        /// runs in one.
        pub fn assign(&mut self, child: &Child) {
            if self.job == 0 {
                return;
            }
            if let Some(h) = child.raw_handle() {
                // SAFETY: valid job + live child process handle.
                let ok = unsafe { AssignProcessToJobObject(self.job as HANDLE, h as HANDLE) };
                if ok == 0 {
                    tracing::warn!("process-tree guard: AssignProcessToJobObject failed — worker descendants may orphan");
                }
            }
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            if self.job != 0 {
                // Closing the last handle triggers KILL_ON_JOB_CLOSE, terminating
                // every process still in the job (the worker + its whole tree).
                unsafe { CloseHandle(self.job as HANDLE) };
            }
        }
    }
}

// ── Unix: process group (killpg on drop) ─────────────────────────────────────
#[cfg(unix)]
mod imp {
    use super::*;

    pub struct ProcessTreeGuard {
        pgid: Option<i32>,
    }

    impl ProcessTreeGuard {
        pub fn prepare(cmd: &mut Command) -> Self {
            // The child leads a NEW process group (pgid == child pid), so a single
            // signal reaches the worker and everything it spawns.
            cmd.process_group(0);
            Self { pgid: None }
        }

        pub fn assign(&mut self, child: &Child) {
            // With process_group(0) the child's pid is its group's pgid.
            self.pgid = child.id().map(|id| id as i32);
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            if let Some(pgid) = self.pgid {
                // SIGKILL the whole group — reaps the worker + any http.server it
                // left running. Best-effort (the group may already be gone).
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
        }
    }
}

// ── Other targets: no-op ─────────────────────────────────────────────────────
#[cfg(not(any(windows, unix)))]
mod imp {
    use super::*;
    pub struct ProcessTreeGuard;
    impl ProcessTreeGuard {
        pub fn prepare(_cmd: &mut Command) -> Self { Self }
        pub fn assign(&mut self, _child: &Child) {}
    }
}
