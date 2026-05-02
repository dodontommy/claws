//! Thin wrapper around the `git` CLI for the worktree-aware spawn flow.
//!
//! claws doesn't track worktrees itself — git already does, perfectly. We
//! shell out, parse `--porcelain`, and surface what we find. This keeps
//! claws's data model simple (worktrees can be created/moved/deleted by
//! the user from any other tool and we'll see the same view next time).
//!
//! All operations refuse to run if `git` isn't on PATH, returning a clear
//! error rather than silently no-oping.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Short branch name (e.g. "main", "feat/x"). None for detached HEAD.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub bare: bool,
    pub locked: bool,
}

/// Branch name at HEAD inside `cwd`. Returns:
/// - `None` if `cwd` is not in a git repo (or `git` isn't available)
/// - `Some("(detached)")` if HEAD is detached
/// - `Some("<branch>")` otherwise
pub fn current_branch(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed == "HEAD" {
        Some("(detached)".to_string())
    } else {
        Some(trimmed.to_string())
    }
}

/// Resolve the working-tree root for a path inside a git repo. Returns None
/// if the path is not inside a git repo (or if `git` isn't available).
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// List all worktrees attached to the repo containing `repo_root`. Order
/// matches git's: main worktree first, then linked worktrees in creation
/// order. On parse failure or git-not-found, returns an empty vec rather
/// than erroring — callers gate the UI on emptiness anyway.
pub fn list_worktrees(repo_root: &Path) -> Vec<Worktree> {
    let out = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let s = String::from_utf8_lossy(&out.stdout);
    parse_worktree_porcelain(&s)
}

/// Public for testing. The porcelain format has one stanza per worktree,
/// stanzas separated by blank lines. Each stanza has lines like:
///   worktree <path>
///   HEAD <oid>
///   branch refs/heads/<name>     (omitted when detached)
///   bare                         (only for bare repos)
///   detached                     (only for detached HEAD)
///   locked [<reason>]            (when locked)
pub fn parse_worktree_porcelain(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;

    let flush = |cur: &mut Option<Worktree>, out: &mut Vec<Worktree>| {
        if let Some(w) = cur.take() {
            out.push(w);
        }
    };

    for line in text.lines() {
        if line.is_empty() {
            flush(&mut cur, &mut out);
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().unwrap_or("");
        let val = parts.next().unwrap_or("");
        match key {
            "worktree" => {
                flush(&mut cur, &mut out);
                cur = Some(Worktree {
                    path: PathBuf::from(val),
                    branch: None,
                    head: None,
                    bare: false,
                    locked: false,
                });
            }
            "HEAD" => {
                if let Some(w) = cur.as_mut() {
                    w.head = Some(val.to_string());
                }
            }
            "branch" => {
                if let Some(w) = cur.as_mut() {
                    let short = val.strip_prefix("refs/heads/").unwrap_or(val).to_string();
                    w.branch = Some(short);
                }
            }
            "bare" => {
                if let Some(w) = cur.as_mut() {
                    w.bare = true;
                }
            }
            "locked" => {
                if let Some(w) = cur.as_mut() {
                    w.locked = true;
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Does `branch` resolve to a commit in the repo rooted at `repo_root`?
/// Used to decide between `git worktree add -b <new>` (new branch) and
/// `git worktree add <path> <existing>` (reuse existing branch).
pub fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a worktree. If `branch` already exists, checks it out at `path`;
/// otherwise creates a new branch off HEAD. Refuses if `path` already
/// exists as a non-empty directory or as a file.
pub fn create_worktree(repo_root: &Path, branch: &str, path: &Path) -> Result<()> {
    if path.exists() {
        if path.is_file() {
            anyhow::bail!("path exists and is a file: {}", path.display());
        }
        let mut entries = std::fs::read_dir(path)
            .with_context(|| format!("read_dir {}", path.display()))?;
        if entries.next().is_some() {
            anyhow::bail!("path exists and is non-empty: {}", path.display());
        }
    }
    let path_str = path.to_string_lossy().into_owned();
    let mut cmd = Command::new("git");
    cmd.arg("worktree").arg("add");
    if branch_exists(repo_root, branch) {
        cmd.arg(&path_str).arg(branch);
    } else {
        cmd.arg("-b").arg(branch).arg(&path_str);
    }
    let out = cmd
        .current_dir(repo_root)
        .output()
        .context("run git worktree add")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(anyhow!(
            "git worktree add failed: {}",
            if !stderr.is_empty() {
                stderr.trim().to_string()
            } else {
                stdout.trim().to_string()
            }
        ));
    }
    Ok(())
}

/// Suggest a default branch name for "create worktree" given existing
/// worktrees on the same repo. We just dedupe with -2/-3/... — no
/// cleverness around dates or task ids.
pub fn suggest_branch_name(repo_basename: &str, existing: &[Worktree]) -> String {
    let used: std::collections::HashSet<String> = existing
        .iter()
        .filter_map(|w| w.branch.clone())
        .collect();
    if !used.contains(repo_basename) {
        return repo_basename.to_string();
    }
    for n in 2u32..1000 {
        let candidate = format!("{repo_basename}-{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    format!("{repo_basename}-{}", uuid::Uuid::new_v4().simple())
}

/// Suggest a default worktree path given the repo root, the new branch
/// name, and existing worktrees. Tries `<parent>/<repo>-<branch>` first
/// and adds `-2`, `-3`, ... if that path already exists in the worktree
/// list or on disk.
pub fn suggest_worktree_path(repo_root: &Path, branch: &str, existing: &[Worktree]) -> PathBuf {
    let parent = repo_root.parent().unwrap_or(repo_root).to_path_buf();
    let repo_basename = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("worktree");
    // Sanitize branch for filesystem path: replace path separators.
    let safe_branch = branch.replace('/', "-").replace('\\', "-");
    let used: std::collections::HashSet<PathBuf> =
        existing.iter().map(|w| w.path.clone()).collect();
    let base = parent.join(format!("{repo_basename}-{safe_branch}"));
    if !used.contains(&base) && !base.exists() {
        return base;
    }
    for n in 2u32..1000 {
        let candidate = parent.join(format!("{repo_basename}-{safe_branch}-{n}"));
        if !used.contains(&candidate) && !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!(
        "{repo_basename}-{safe_branch}-{}",
        uuid::Uuid::new_v4().simple()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_main_only() {
        let text = "worktree /home/x/repo\nHEAD abcdef\nbranch refs/heads/main\n\n";
        let v = parse_worktree_porcelain(text);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, PathBuf::from("/home/x/repo"));
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert_eq!(v[0].head.as_deref(), Some("abcdef"));
        assert!(!v[0].bare);
    }

    #[test]
    fn parse_porcelain_multiple() {
        let text = "\
worktree /home/x/repo
HEAD aaa
branch refs/heads/main

worktree /home/x/repo-feat
HEAD bbb
branch refs/heads/feat/cool

worktree /home/x/repo-detached
HEAD ccc
detached
";
        let v = parse_worktree_porcelain(text);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].branch.as_deref(), Some("main"));
        assert_eq!(v[1].branch.as_deref(), Some("feat/cool"));
        assert!(v[2].branch.is_none());
    }

    #[test]
    fn parse_porcelain_empty() {
        assert_eq!(parse_worktree_porcelain("").len(), 0);
    }

    #[test]
    fn parse_porcelain_locked_and_bare() {
        let text = "\
worktree /home/x/repo
HEAD aaa
bare

worktree /home/x/repo-locked
HEAD bbb
branch refs/heads/x
locked some reason
";
        let v = parse_worktree_porcelain(text);
        assert_eq!(v.len(), 2);
        assert!(v[0].bare);
        assert!(v[1].locked);
    }

    #[test]
    fn suggest_branch_unused_repo_basename() {
        let v: Vec<Worktree> = vec![];
        assert_eq!(suggest_branch_name("claws", &v), "claws");
    }

    #[test]
    fn suggest_branch_dedupes() {
        let v = vec![
            Worktree {
                path: PathBuf::from("/x/claws"),
                branch: Some("claws".to_string()),
                head: None,
                bare: false,
                locked: false,
            },
            Worktree {
                path: PathBuf::from("/x/claws-2"),
                branch: Some("claws-2".to_string()),
                head: None,
                bare: false,
                locked: false,
            },
        ];
        assert_eq!(suggest_branch_name("claws", &v), "claws-3");
    }

    #[test]
    fn suggest_path_basic() {
        let p = suggest_worktree_path(Path::new("/x/claws"), "feat-x", &[]);
        assert_eq!(p, PathBuf::from("/x/claws-feat-x"));
    }

    #[test]
    fn suggest_path_sanitizes_slashes_in_branch() {
        let p = suggest_worktree_path(Path::new("/x/claws"), "feat/x", &[]);
        assert_eq!(p, PathBuf::from("/x/claws-feat-x"));
    }
}
