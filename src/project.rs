//! Derive project metadata from a session's working directory.
//!
//! For a given absolute `cwd` path, walk ancestors looking for a
//! `.git` entry (directory or file — the latter being the worktree
//! convention).  The nearest ancestor containing one is the project
//! root.  From that root we split out the four values consumed by the
//! `token-usage` action:
//!
//! - `cwd` — the original full path, untouched.
//! - `project_path` — the **parent directory** of the project root,
//!   with `$HOME` collapsed to `~` for display.  When no `.git` is
//!   found, this falls back to `cwd` itself so users still see a
//!   path.
//! - `project` — the **basename** of the project root.  Empty when
//!   no `.git` is found, so users can filter "non-git sessions" via
//!   `--project=""`.
//! - `subpath` — `cwd` relative to the project root.  Empty when
//!   `cwd` IS the project root, or when no `.git` is found.
//!
//! ## Why split basename and parent
//!
//! The split makes the `project` field stable and short enough to
//! filter on (`-p ai-audit` rather than `-p /home/vaab/dev/rs/ai-audit`),
//! while preserving the location context (`project_path = ~/dev/rs`)
//! for disambiguation between same-named projects in different
//! locations.

use std::path::{Path, PathBuf};

/// Project metadata derived from a session's working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    /// The original full working directory of the session.
    pub cwd: PathBuf,
    /// Parent directory of the `.git`-ancestor (with `$HOME` → `~`).
    /// Falls back to `cwd` when no `.git` is found.
    pub project_path: PathBuf,
    /// Basename of the `.git`-ancestor.  Empty when no `.git` found.
    pub project: String,
    /// `cwd` relative to the `.git`-ancestor.  Empty when `cwd` is
    /// the project root, or when no `.git` is found.
    pub subpath: PathBuf,
}

/// Derive project metadata from an absolute `cwd` path.
///
/// Walks `cwd.ancestors()` (starting from `cwd` itself) and stops at
/// the first ancestor where `<ancestor>/.git` exists — as either a
/// directory (regular clone) or a file (git worktree pointer).
///
/// When no `.git` is found:
/// - `project` = `""`
/// - `subpath` = `""`
/// - `project_path` = `cwd` (with `$HOME` → `~` collapse)
/// - `cwd` = original path unchanged
pub fn project_info_from_cwd(cwd: &Path) -> ProjectInfo {
    let cwd_buf = cwd.to_path_buf();

    // Walk ancestors looking for .git (file OR directory).
    let git_root = cwd
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(|p| p.to_path_buf());

    match git_root {
        Some(root) => {
            let project = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let parent = root
                .parent()
                .map(collapse_home)
                .unwrap_or_else(|| collapse_home(&root));
            let subpath = cwd
                .strip_prefix(&root)
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            ProjectInfo {
                cwd: cwd_buf,
                project_path: parent,
                project,
                subpath,
            }
        }
        None => ProjectInfo {
            cwd: cwd_buf.clone(),
            project_path: collapse_home(&cwd_buf),
            project: String::new(),
            subpath: PathBuf::new(),
        },
    }
}

/// Replace a leading `$HOME` segment with `~`.
///
/// If `$HOME` cannot be resolved, the path is returned unchanged.
fn collapse_home(path: &Path) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => PathBuf::from("~"),
        Ok(rest) => PathBuf::from("~").join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a fake git repo at `<tmp>/<name>/.git/` (as a directory)
    /// and return both the tmp guard and the repo path.
    fn fake_git_repo_dir(parent: &Path, name: &str) -> PathBuf {
        let repo = parent.join(name);
        fs::create_dir_all(repo.join(".git")).unwrap();
        repo
    }

    /// Create a fake git worktree at `<tmp>/<name>/` whose `.git` is
    /// a *file* (the worktree pointer convention).
    fn fake_git_worktree(parent: &Path, name: &str) -> PathBuf {
        let repo = parent.join(name);
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join(".git"), "gitdir: /elsewhere\n").unwrap();
        repo
    }

    #[test]
    fn cwd_inside_repo_with_subfolder() {
        let tmp = TempDir::new().unwrap();
        let repo = fake_git_repo_dir(tmp.path(), "myproj");
        let inner = repo.join("src/sub");
        fs::create_dir_all(&inner).unwrap();

        let info = project_info_from_cwd(&inner);
        assert_eq!(info.cwd, inner);
        assert_eq!(info.project, "myproj");
        assert_eq!(info.subpath, PathBuf::from("src/sub"));
        // project_path is the parent of the repo (some tmp dir).
        // We can't assert the exact value (tmp varies), but it must
        // equal the tmp path with $HOME collapse applied.
        assert_eq!(info.project_path, collapse_home(tmp.path()));
    }

    #[test]
    fn cwd_at_repo_root() {
        let tmp = TempDir::new().unwrap();
        let repo = fake_git_repo_dir(tmp.path(), "myproj");

        let info = project_info_from_cwd(&repo);
        assert_eq!(info.project, "myproj");
        assert_eq!(info.subpath, PathBuf::new());
        assert_eq!(info.project_path, collapse_home(tmp.path()));
    }

    #[test]
    fn no_git_ancestor() {
        // Use a synthetic non-existent path so no `.git` is ever
        // found in the ancestor walk.  Constructing a real tempdir
        // under `/tmp` is unreliable here because `/tmp` itself can
        // be a git repo on some development machines (it is on this
        // user's machine, where `/tmp/.git` exists), causing the
        // walk to ascend out of the tempdir and "find" a project.
        let synthetic = PathBuf::from("/nonexistent-zzz-xyz/notes/scratch");

        let info = project_info_from_cwd(&synthetic);
        assert_eq!(info.project, "");
        assert_eq!(info.subpath, PathBuf::new());
        // project_path falls back to cwd (with $HOME collapse).
        assert_eq!(info.project_path, collapse_home(&synthetic));
        assert_eq!(info.cwd, synthetic);
    }

    #[test]
    fn nested_repos_innermost_wins() {
        let tmp = TempDir::new().unwrap();
        let outer = fake_git_repo_dir(tmp.path(), "outer");
        let inner = fake_git_repo_dir(&outer, "inner");
        let deep = inner.join("src");
        fs::create_dir_all(&deep).unwrap();

        let info = project_info_from_cwd(&deep);
        assert_eq!(info.project, "inner");
        assert_eq!(info.subpath, PathBuf::from("src"));
        assert_eq!(info.project_path, collapse_home(&outer));
    }

    #[test]
    fn git_as_file_worktree() {
        let tmp = TempDir::new().unwrap();
        let wt = fake_git_worktree(tmp.path(), "worktree-A");
        let inside = wt.join("src");
        fs::create_dir_all(&inside).unwrap();

        let info = project_info_from_cwd(&inside);
        assert_eq!(info.project, "worktree-A");
        assert_eq!(info.subpath, PathBuf::from("src"));
        assert_eq!(info.project_path, collapse_home(tmp.path()));
    }

    #[test]
    fn home_collapsed_to_tilde() {
        // Synthetic path under $HOME — we don't need a real git
        // repo, just verify the collapse logic on an arbitrary path.
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let synthetic = home.join("some/where");
        let collapsed = collapse_home(&synthetic);
        assert_eq!(collapsed, PathBuf::from("~/some/where"));
    }

    #[test]
    fn home_itself_collapses_to_just_tilde() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(collapse_home(&home), PathBuf::from("~"));
    }

    #[test]
    fn path_outside_home_unchanged() {
        let outside = PathBuf::from("/var/tmp/somewhere");
        assert_eq!(collapse_home(&outside), outside);
    }
}
