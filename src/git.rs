//! Git integration: runs git to produce diffs and resolves repository layout
//! so the app works in normal repos, linked worktrees, and bare repos.

use std::path::PathBuf;
use std::process::Command;

/// What to diff.
#[derive(Debug, Clone)]
pub enum Source {
    /// Read a diff straight from stdin (pager mode).
    Stdin,
    /// Working tree vs index (`git diff`).
    Worktree,
    /// Index vs HEAD (`git diff --staged`).
    Staged,
    /// A single commit (`git show`), or a revision range `a..b`.
    Rev(String),
}

/// Viewer-safe knobs for how git renders the diff. These only tune the unified
/// patch (never switch to a non-viewable format), plus an optional pathspec.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub ignore_whitespace: bool,
    pub context: Option<usize>,
    pub algorithm: Option<String>,
    pub pathspec: Vec<String>,
}

impl Opts {
    fn flags(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.ignore_whitespace {
            v.push("-w".into());
        }
        if let Some(n) = self.context {
            v.push(format!("-U{n}"));
        }
        if let Some(a) = &self.algorithm {
            v.push(format!("--diff-algorithm={a}"));
        }
        v
    }
}

/// Resolved repository paths used for watching.
#[derive(Debug, Clone)]
pub struct Repo {
    /// Per-worktree git dir (holds index, HEAD). Absolute.
    pub git_dir: PathBuf,
    /// Shared common dir (holds refs, packed-refs, logs). Absolute.
    pub common_dir: PathBuf,
    pub is_bare: bool,
}

fn git(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git").args(args).output()
}

/// Resolve the repository layout for the current directory, or None if not a
/// git repo.
pub fn discover() -> Option<Repo> {
    let out = git(&["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir", "--is-bare-repository"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.lines();
    let git_dir = PathBuf::from(it.next()?.trim());
    let common_dir = PathBuf::from(it.next().unwrap_or("").trim());
    let is_bare = it.next().map(|s| s.trim() == "true").unwrap_or(false);
    let common_dir = if common_dir.as_os_str().is_empty() {
        git_dir.clone()
    } else {
        common_dir
    };
    Some(Repo {
        git_dir,
        common_dir,
        is_bare,
    })
}

/// Absolute path to the working-tree root, or None for bare repos.
pub fn toplevel() -> Option<PathBuf> {
    let out = git(&["rev-parse", "--show-toplevel"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

impl Source {
    /// Produce the unified diff text for this source, tuned by `opts`.
    pub fn diff(&self, opts: &Opts) -> std::io::Result<String> {
        if let Source::Stdin = self {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            return Ok(s);
        }
        // Force a stable, parseable format regardless of user git config.
        let mut a: Vec<String> = vec![
            "-c".into(),
            "core.pager=cat".into(),
            "-c".into(),
            "color.diff=never".into(),
        ];
        match self {
            Source::Stdin => unreachable!(),
            Source::Worktree => {
                a.extend(["diff", "--no-color", "--no-ext-diff"].map(String::from));
                a.extend(opts.flags());
            }
            Source::Staged => {
                a.extend(["diff", "--no-color", "--no-ext-diff", "--staged"].map(String::from));
                a.extend(opts.flags());
            }
            Source::Rev(rev) => {
                if rev.contains("..") {
                    a.extend(["diff", "--no-color", "--no-ext-diff"].map(String::from));
                } else {
                    a.extend(
                        ["show", "--no-color", "--no-ext-diff", "--format=fuller"].map(String::from),
                    );
                }
                a.extend(opts.flags());
                a.push(rev.clone());
            }
        }
        if !opts.pathspec.is_empty() {
            a.push("--".into());
            a.extend(opts.pathspec.iter().cloned());
        }
        let out = Command::new("git").args(&a).output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(std::io::Error::other(format!("git failed: {}", err.trim())));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Whether this source reflects unstaged working-tree edits, which touch
    /// neither the index nor refs and so escape the git-internals watcher.
    /// Only these need the polling fallback in watch mode.
    pub fn reads_worktree(&self) -> bool {
        matches!(self, Source::Worktree)
    }
}
