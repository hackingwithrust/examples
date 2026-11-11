//! Git logic for `gitdesk`, the desktop Git client built step by step in
//! the [A Git client in Rust](https://hackingwithrust.com/projects/gitdesk/)
//! track.
//!
//! The crate fills in one track step at a time. Step 2 opens a repository
//! and reads its four stores; step 6 graduates the line diff — Myers'
//! edit script and the unified hunks it prints as — into `diff`, the
//! first logic to live here. Everything else still lives in
//! `examples/` until a second feature needs it.

pub mod diff;
