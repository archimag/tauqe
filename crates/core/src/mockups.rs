//! The `.tauqe/mockups/` sandbox for throwaway prototypes and drafts.
//!
//! The sandbox path and its predicate are deterministic substrate and live in
//! `tauqe_tools::mockups`, beside the write admission that enforces the same
//! rule. The re-export keeps `crate::mockups::is_mockups_path` resolvable for the
//! engine while the transition lasts.

pub use tauqe_tools::mockups::{is_mockups_path, MOCKUPS_DIR};
