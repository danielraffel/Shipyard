//! Executor support modules.
//!
//! These modules hold transport-specific contracts that can be tested
//! without reaching a remote machine. Live process orchestration should
//! call into these helpers instead of rebuilding shell strings inline.

/// PowerShell CLIXML decoding for Windows SSH stderr.
pub mod clixml;
/// GitHub Actions cloud executor.
pub mod cloud;
/// Validation contract evaluation.
pub mod contract;
/// Backend-aware executor dispatch.
pub mod dispatch;
/// Local executor planning helpers.
pub mod local;
/// POSIX SSH command and probe contracts.
pub mod ssh;
/// Windows SSH PowerShell command construction.
pub mod ssh_windows;
/// Shared subprocess streaming helpers for validation executors.
pub mod streaming;
/// Ownership and reclamation of validation TMPDIRs.
pub mod validation_tmp;
