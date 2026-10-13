//! `shipyard inspect`: read a repository's CI and say what to change.
//!
//! Inspect is an advisor. It runs only when asked, reads GitHub and the
//! repository's own files, and never sits on the path of `ship`, `pr`, the
//! guards, the daemon or any gate step. Every check declares which facts it
//! applies to, and those facts come from [`profile`]: what the project
//! actually builds with, pins, runs on and tests with, each with the file
//! line or API row it was read from.

pub mod profile;
