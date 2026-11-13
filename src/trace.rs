//! Lightweight IO tracing for cache correctness.
//!
//! When enabled (`--trace-io`), yatr snapshots the working tree around a task's
//! execution and reports files it **wrote outside its declared `outputs`**. That
//! is the most common silent cache bug: a task produces an artifact it didn't
//! declare, so a later cache hit won't restore it.
//!
//! This is deliberately portable and cheap — it respects `.gitignore` (so it
//! skips `target/`, `node_modules/`, `.git/`) and compares file size + mtime
//! rather than tracing syscalls. The trade-off: writes into gitignored
//! directories aren't seen (full read/write tracing needs OS-level tooling).

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use globset::GlobSetBuilder;
use ignore::WalkBuilder;

use crate::globs;

/// A cheap fingerprint of the working tree: relative path -> (mtime, size).
pub type Snapshot = HashMap<String, (SystemTime, u64)>;

/// Snapshot the files under `cwd` (honouring `.gitignore`).
#[must_use]
pub fn snapshot(cwd: &Path) -> Snapshot {
    let mut map = Snapshot::new();
    for entry in WalkBuilder::new(cwd)
        .build()
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        if let Ok(md) = path.metadata() {
            let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            // Normalise to forward slashes so output-glob/prefix checks are
            // consistent across platforms (Windows uses `\`).
            let key = rel.to_string_lossy().replace('\\', "/");
            map.insert(key, (mtime, md.len()));
        }
    }
    map
}

/// Relative paths of files that were created or modified between `before` and
/// `after` and are **not** covered by any of the task's `outputs` patterns.
#[must_use]
pub fn undeclared_writes(before: &Snapshot, after: &Snapshot, outputs: &[String]) -> Vec<String> {
    let mut builder = GlobSetBuilder::new();
    for o in outputs {
        if let Ok(g) = globs::compile(o) {
            builder.add(g);
        }
    }
    let globs = builder.build().ok();

    let covered = |rel: &str| -> bool {
        // A literal output, a file under a declared output directory, or a glob match.
        outputs
            .iter()
            .map(|o| globs::normalise(o).trim_end_matches('/'))
            .any(|o| rel == o || rel.starts_with(&format!("{o}/")))
            || globs.as_ref().is_some_and(|g| g.is_match(rel))
    };

    let mut writes: Vec<String> = after
        .iter()
        .filter(|(rel, sig)| before.get(*rel).is_none_or(|b| b != *sig) && !covered(rel))
        .map(|(rel, _)| rel.clone())
        .collect();
    writes.sort();
    writes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_writes_outside_declared_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();

        let before = snapshot(cwd);
        std::fs::write(cwd.join("declared.bin"), b"x").unwrap();
        std::fs::create_dir_all(cwd.join("dist")).unwrap();
        std::fs::write(cwd.join("dist/app.js"), b"y").unwrap();
        std::fs::write(cwd.join("stray.tmp"), b"z").unwrap();
        let after = snapshot(cwd);

        // `dist` (dir) and `declared.bin` are declared; `stray.tmp` is not.
        let undeclared =
            undeclared_writes(&before, &after, &["declared.bin".into(), "dist".into()]);
        assert_eq!(undeclared, vec!["stray.tmp".to_string()]);
    }

    #[test]
    fn unchanged_tree_has_no_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        let before = snapshot(dir.path());
        let after = snapshot(dir.path());
        assert!(undeclared_writes(&before, &after, &[]).is_empty());
    }
}
