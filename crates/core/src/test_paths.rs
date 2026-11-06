//! Deterministic scratch paths for the unit tests of this crate.
//!
//! Every test temporary directory is derived from one rule:
//! `<temp>/tauqe-<prefix>-<pid>-<counter>`. The process identifier isolates the
//! parallel test processes a runner such as `cargo nextest` starts — each of
//! them restarts the process-global counters — while the atomic counter
//! isolates the parallel tests of one `cargo test` process. A name derived from
//! a counter or a clock reading alone collides between processes, so it is a
//! defect rather than a simplification.
//!
//! The module is compiled under `cfg(test)` only, so it derives no production
//! path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Sequence number of the scratch directories handed out by this process.
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a unique scratch directory and returns its path.
///
/// The directory exists and is empty when the path is returned, so a caller may
/// write into it immediately. The name carries the process identifier and a
/// process-local sequence number, so two tests never share a directory, whether
/// they run as threads of one process or as separate test processes.
pub(crate) fn scratch_dir(prefix: &str) -> PathBuf {
    let seq = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tauqe-{}-{}-{}", prefix, std::process::id(), seq));
    // A directory an interrupted run left behind under the same process
    // identifier must not leak its residue into this test.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

mod tests {
    use super::scratch_dir;

    /// The derived name carries the prefix and the process identifier, which is
    /// what keeps concurrently running test processes apart.
    #[test]
    fn the_path_names_the_prefix_and_the_process() {
        let dir = scratch_dir("naming");
        assert!(dir.is_dir(), "the helper creates the directory");
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("the derived name is valid UTF-8");
        assert!(name.starts_with("tauqe-naming-"), "{name}");
        let pid = format!("-{}-", std::process::id());
        assert!(
            name.contains(&pid),
            "the name carries the process identifier: {name}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two calls never hand out the same directory, which is what keeps the
    /// parallel tests of one process apart.
    #[test]
    fn consecutive_calls_yield_distinct_directories() {
        let first = scratch_dir("sequence");
        let second = scratch_dir("sequence");
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }
}
