//! Minimal git plumbing for `ctx changed`: resolve the repo root and list
//! the files that differ from a ref (or the working tree), so a diff can be
//! mapped onto the module graph. Shells out to `git`; no dependency.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

fn git(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("failed to run `git` (is it installed and on PATH?)")
}

fn rev_exists(root: &Path, rev: &str) -> bool {
    git(root, &["rev-parse", "--verify", "-q", rev])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The repository root containing `path`.
pub fn repo_root(path: &Path) -> Result<PathBuf> {
    let out = git(path, &["rev-parse", "--show-toplevel"])?;
    if !out.status.success() {
        bail!("not a git repository: {}", path.display());
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(PathBuf::from(root))
}

fn collect(stdout: &[u8], root: &Path, out: &mut Vec<PathBuf>) {
    for line in String::from_utf8_lossy(stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            out.push(root.join(line));
        }
    }
}

/// Absolute paths of files that differ from `since` (or from HEAD when
/// `since` is None), plus untracked files. An unborn HEAD (no commits yet)
/// yields just the untracked set rather than an error.
pub fn changed_files(path: &Path, since: Option<&str>) -> Result<Vec<PathBuf>> {
    let root = repo_root(path)?;
    let mut files = Vec::new();

    let base = match since {
        Some(r) => Some(r.to_string()),
        None => rev_exists(&root, "HEAD").then(|| "HEAD".to_string()),
    };
    if let Some(base) = base {
        let out = git(&root, &["diff", "--name-only", &base, "--"])?;
        if !out.status.success() {
            bail!(
                "git diff against '{}' failed: {}",
                base,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        collect(&out.stdout, &root, &mut files);
    }

    // Untracked (newly added) files are changes not in any commit.
    let out = git(&root, &["ls-files", "--others", "--exclude-standard"])?;
    collect(&out.stdout, &root, &mut files);

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn init_repo(files: &[(&str, &str)]) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ctx_git_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        for (rel, content) in files {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, content).unwrap();
        }
        dir
    }

    fn commit_all(dir: &Path) {
        git(dir, &["add", "-A"]).unwrap();
        git(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "x",
            ],
        )
        .unwrap();
    }

    #[test]
    fn untracked_file_is_reported() {
        let dir = init_repo(&[("src/a.rs", "pub fn a() {}\n")]);
        let changed = changed_files(&dir, None).unwrap();
        assert!(changed.iter().any(|p| p.ends_with("src/a.rs")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn modified_tracked_file_is_reported() {
        let dir = init_repo(&[("src/a.rs", "pub fn a() {}\n")]);
        commit_all(&dir);
        fs::write(dir.join("src/a.rs"), "pub fn a() { let _ = 1; }\n").unwrap();
        let changed = changed_files(&dir, None).unwrap();
        assert!(changed.iter().any(|p| p.ends_with("src/a.rs")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn clean_tree_reports_nothing() {
        let dir = init_repo(&[("src/a.rs", "pub fn a() {}\n")]);
        commit_all(&dir);
        assert!(changed_files(&dir, None).unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
