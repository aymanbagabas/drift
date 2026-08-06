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
    /// Also show untracked (non-ignored) files, each diffed against an empty
    /// blob. Only meaningful for the worktree source.
    pub all: bool,
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
            // Git colorizes the diff it pipes to a pager; strip it so the
            // parser sees plain text.
            return Ok(uncurses::ansi::strip::strip(&s));
        }
        // Force a stable, parseable format regardless of user git config.
        let mut a: Vec<String> = vec![
            "-c".into(),
            "core.pager=cat".into(),
            "-c".into(),
            "color.diff=never".into(),
            // Keep the plain a/ b/ path prefixes; diff.mnemonicPrefix would
            // emit i/ w/ c/ o/ instead, which breaks path display and open.
            "-c".into(),
            "diff.mnemonicPrefix=false".into(),
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
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        // `-A` on the worktree also surfaces untracked files, which plain
        // `git diff` omits. Append them as new-file diffs.
        if opts.all && matches!(self, Source::Worktree) {
            text.push_str(&untracked_diff(opts)?);
        }
        Ok(text)
    }

    /// Whether this source reflects unstaged working-tree edits, which touch
    /// neither the index nor refs and so escape the git-internals watcher.
    /// Only these need the polling fallback in watch mode.
    pub fn reads_worktree(&self) -> bool {
        matches!(self, Source::Worktree)
    }
}

/// Diff every untracked, non-ignored file against an empty input so `-A`
/// shows brand-new files that plain `git diff` skips. Honors the same
/// pathspec and render flags as the main diff. `--no-index` exits non-zero
/// when it finds differences (always, here), so its status is ignored.
// ponytail: uses /dev/null (POSIX); a Windows port would need "NUL".
fn untracked_diff(opts: &Opts) -> std::io::Result<String> {
    let mut ls: Vec<String> = ["ls-files", "--others", "--exclude-standard", "-z"]
        .map(String::from)
        .into();
    if !opts.pathspec.is_empty() {
        ls.push("--".into());
        ls.extend(opts.pathspec.iter().cloned());
    }
    let listed = git(&ls.iter().map(String::as_str).collect::<Vec<_>>())?;
    if !listed.status.success() {
        return Ok(String::new());
    }
    let names = String::from_utf8_lossy(&listed.stdout);
    let mut text = String::new();
    for file in names.split('\0').filter(|s| !s.is_empty()) {
        let mut a: Vec<String> = [
            "-c",
            "diff.mnemonicPrefix=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-index",
        ]
        .map(String::from)
        .into();
        a.extend(opts.flags());
        a.push("/dev/null".into());
        a.push(file.to_string());
        let out = Command::new("git").args(&a).output()?;
        text.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-A` surfaces an untracked file as a new-file diff with clean `a/ b/`
    /// prefixes, on top of the tracked change.
    #[test]
    fn all_includes_untracked() {
        let dir = std::env::temp_dir().join(format!("drift-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t.co"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "brand new\n").unwrap();

        let opts = Opts { all: true, ..Default::default() };
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let text = Source::Worktree.diff(&opts);
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let text = text.unwrap();
        assert!(text.contains("b/tracked.txt"), "tracked change missing:\n{text}");
        assert!(text.contains("b/fresh.txt"), "untracked file missing:\n{text}");
        assert!(text.contains("new file"), "no new-file header:\n{text}");
    }
}
