//! Resident-memory sampling for language-server process trees .
//!
//! rust-analyzer alone can hold gigabytes, and it spawns helpers (proc-macro
//! servers) that hold more, so the number that matters is the RSS of the whole
//! tree under the server's pid. Linux exposes it through `/proc`; elsewhere no
//! sample is available and the memory guard simply does not act.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Most processes examined per tree, so a fork bomb cannot stall sampling.
const MAX_TREE_PROCESSES: usize = 512;

/// Something that can report the resident memory of a process, or of a tree of
/// processes.
pub trait MemorySampler: Send + Sync + std::fmt::Debug {
    /// Bytes of resident memory of `pid` and all its descendants, or `None`
    /// when `pid` cannot be sampled (gone, or the platform has no support).
    ///
    /// This is the figure for a *language server*: its helpers (rust-analyzer's
    /// proc-macro servers) are its children, so a tree is the honest number.
    fn tree_rss_bytes(&self, pid: u32) -> Option<u64>;

    /// Bytes of resident memory of `pid` **alone**, descendants excluded, or
    /// `None` when `pid` cannot be sampled.
    ///
    /// This is the figure for the *daemon*: the language servers it supervises
    /// are its children and are already covered by [`Self::tree_rss_bytes`]
    /// against their own ceiling, so counting them here would both double-count
    /// the same memory and hide the leak this reading exists to catch.
    fn self_rss_bytes(&self, pid: u32) -> Option<u64>;
}

/// Reads `/proc`. The root is injectable so tests can build a fake tree.
#[derive(Debug, Clone)]
pub struct ProcSampler {
    root: PathBuf,
}

impl Default for ProcSampler {
    fn default() -> Self {
        Self::with_root("/proc")
    }
}

impl ProcSampler {
    /// A sampler reading a proc-like tree rooted at `root`.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn rss_of(&self, pid: u32) -> Option<u64> {
        let status =
            std::fs::read_to_string(self.root.join(pid.to_string()).join("status")).ok()?;
        parse_vm_rss_bytes(&status)
    }

    fn children_of(&self, pid: u32) -> Vec<u32> {
        let tasks = self.root.join(pid.to_string()).join("task");
        let Ok(entries) = std::fs::read_dir(tasks) else {
            return Vec::new();
        };
        let mut children = Vec::new();
        for entry in entries.flatten() {
            if let Ok(text) = std::fs::read_to_string(entry.path().join("children")) {
                children.extend(
                    text.split_whitespace()
                        .filter_map(|p| p.parse::<u32>().ok()),
                );
            }
        }
        children
    }
}

impl MemorySampler for ProcSampler {
    fn tree_rss_bytes(&self, pid: u32) -> Option<u64> {
        // The root must be sampleable; a descendant that vanished mid-walk is
        // just skipped.
        let mut total = self.rss_of(pid)?;
        let mut seen: HashSet<u32> = HashSet::from([pid]);
        let mut queue = self.children_of(pid);
        while let Some(next) = queue.pop() {
            if seen.len() >= MAX_TREE_PROCESSES {
                tracing::warn!(pid, "process tree too large; memory sample is partial");
                break;
            }
            if !seen.insert(next) {
                continue;
            }
            if let Some(rss) = self.rss_of(next) {
                total += rss;
            }
            queue.extend(self.children_of(next));
        }
        Some(total)
    }

    fn self_rss_bytes(&self, pid: u32) -> Option<u64> {
        self.rss_of(pid)
    }
}

/// The `VmRSS:` line of a `/proc/<pid>/status`, in bytes.
pub fn parse_vm_rss_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kib: u64 = line
        .trim_start_matches("VmRSS:")
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Where the default sampler looks; exposed so `doctor` can report support.
pub fn proc_available() -> bool {
    Path::new("/proc/self/status").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(kib: u64) -> String {
        format!(
            "Name:\tx\nVmPeak:\t{}\tkB\nVmRSS:\t   {kib} kB\nThreads:\t1\n",
            kib * 2
        )
    }

    /// Builds `<root>/<pid>/status` and `<root>/<pid>/task/<pid>/children`.
    fn add(root: &Path, pid: u32, kib: Option<u64>, children: &[u32]) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(dir.join("task").join(pid.to_string())).unwrap();
        if let Some(kib) = kib {
            std::fs::write(dir.join("status"), status(kib)).unwrap();
        }
        let list = children
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(
            dir.join("task").join(pid.to_string()).join("children"),
            list,
        )
        .unwrap();
    }

    #[test]
    fn vm_rss_is_parsed_in_bytes() {
        assert_eq!(parse_vm_rss_bytes(&status(2048)), Some(2048 * 1024));
        assert_eq!(parse_vm_rss_bytes("Name:\tx\n"), None);
        assert_eq!(parse_vm_rss_bytes("VmRSS:\tlots kB\n"), None);
        assert_eq!(parse_vm_rss_bytes("VmRSS:\n"), None);
    }

    #[test]
    fn a_tree_is_summed_across_children_and_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), 100, Some(1000), &[101, 102]);
        add(dir.path(), 101, Some(200), &[]);
        add(dir.path(), 102, Some(300), &[103]);
        add(dir.path(), 103, Some(40), &[]);
        add(dir.path(), 999, Some(77_777), &[]); // unrelated process
        let sampler = ProcSampler::with_root(dir.path());
        assert_eq!(
            sampler.tree_rss_bytes(100),
            Some((1000 + 200 + 300 + 40) * 1024)
        );
        assert_eq!(sampler.tree_rss_bytes(103), Some(40 * 1024));
    }

    #[test]
    fn children_spread_over_several_threads_are_all_found() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), 10, Some(100), &[11]);
        add(dir.path(), 11, Some(5), &[]);
        add(dir.path(), 12, Some(6), &[]);
        // A second thread of pid 10 also spawned pid 12.
        let second = dir.path().join("10/task/11");
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("children"), "12").unwrap();
        let sampler = ProcSampler::with_root(dir.path());
        assert_eq!(sampler.tree_rss_bytes(10), Some((100 + 5 + 6) * 1024));
    }

    #[test]
    fn a_missing_root_process_is_no_sample() {
        let dir = tempfile::tempdir().unwrap();
        let sampler = ProcSampler::with_root(dir.path());
        assert_eq!(sampler.tree_rss_bytes(4242), None);
    }

    /// The reading the daemon's own ceiling is judged on: this process alone,
    /// however large the children it supervises are.
    #[test]
    fn a_self_reading_excludes_the_children() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), 100, Some(1000), &[101, 102]);
        add(dir.path(), 101, Some(200), &[103]);
        add(dir.path(), 102, Some(300), &[]);
        add(dir.path(), 103, Some(40), &[]);
        let sampler = ProcSampler::with_root(dir.path());
        assert_eq!(sampler.self_rss_bytes(100), Some(1000 * 1024));
        assert_eq!(
            sampler.tree_rss_bytes(100),
            Some((1000 + 200 + 300 + 40) * 1024),
            "the tree reading must keep counting the children"
        );
        assert_eq!(sampler.self_rss_bytes(4242), None);
    }

    #[test]
    fn a_descendant_that_vanished_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), 100, Some(1000), &[101, 102]);
        add(dir.path(), 101, None, &[]); // status already gone
        add(dir.path(), 102, Some(50), &[]);
        let sampler = ProcSampler::with_root(dir.path());
        assert_eq!(sampler.tree_rss_bytes(100), Some((1000 + 50) * 1024));
    }

    #[test]
    fn cycles_in_the_children_lists_terminate() {
        let dir = tempfile::tempdir().unwrap();
        add(dir.path(), 1, Some(10), &[2]);
        add(dir.path(), 2, Some(20), &[1, 2]);
        let sampler = ProcSampler::with_root(dir.path());
        assert_eq!(sampler.tree_rss_bytes(1), Some(30 * 1024));
    }

    #[test]
    fn an_enormous_tree_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let kids: Vec<u32> = (2..(MAX_TREE_PROCESSES as u32 + 100)).collect();
        add(dir.path(), 1, Some(1), &kids);
        for pid in &kids {
            add(dir.path(), *pid, Some(1), &[]);
        }
        let sampler = ProcSampler::with_root(dir.path());
        let total = sampler.tree_rss_bytes(1).unwrap();
        assert!(
            total <= (MAX_TREE_PROCESSES as u64 + 1) * 1024,
            "capped: {total}"
        );
        assert!(total > 100 * 1024);
    }

    #[test]
    fn the_real_proc_reports_this_process() {
        if !proc_available() {
            return;
        }
        let rss = ProcSampler::default()
            .tree_rss_bytes(std::process::id())
            .unwrap();
        assert!(
            rss > 1024 * 1024,
            "a test binary holds more than 1 MiB: {rss}"
        );
    }

    #[test]
    fn a_dead_pid_in_the_real_proc_is_no_sample() {
        // pid 0 never exists as a /proc entry.
        assert_eq!(ProcSampler::default().tree_rss_bytes(0), None);
    }
}
