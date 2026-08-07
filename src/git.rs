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

/// Bytes to buffer while classifying before giving up and treating the stream
/// as non-diff. A real diff preamble (a `git show`/`log` header) is far under
/// this; the cap bounds memory and latency for non-diff pager input.
// ponytail: fixed 64 KiB cap; make it configurable only if a real preamble ever exceeds it.
const PEEK_CAP: usize = 64 * 1024;

/// Peek a piped stream to decide whether it's a unified diff before the TUI
/// takes over the terminal. Reads lines until a `diff --git` header confirmed
/// by a following git patch line (ANSI stripped, since git may colorize), EOF,
/// or `PEEK_CAP` bytes. Returns `(is_diff, consumed)` with the raw bytes read:
/// on `false`, write them back and stream the rest of stdin — it wasn't a diff
/// (e.g. `git diff --stat`); on `true`, hand them to the streamer so the
/// already-read prefix isn't lost. Bytes stay raw and are decoded lossily only
/// for the check, so non-UTF-8 input (a legacy-encoded commit message, a binary
/// stream) passes through instead of aborting.
pub fn peek_diff<R: std::io::BufRead>(mut r: R) -> std::io::Result<(bool, Vec<u8>)> {
    let mut consumed = Vec::new();
    let mut pending_header = false; // saw `diff --git`, awaiting a continuation line
    loop {
        let start = consumed.len();
        if r.read_until(b'\n', &mut consumed)? == 0 {
            return Ok((false, consumed)); // EOF before a confirmed diff.
        }
        let head = strip_head(&consumed[start..]);
        if pending_header {
            // A real `diff --git` is always followed by an `index`/`@@`/mode/
            // rename/binary line; plain prose that merely starts with those
            // words won't be, so it isn't misrouted into the TUI.
            if is_diff_continuation(&head) {
                return Ok((true, consumed));
            }
            pending_header = false; // false alarm — keep scanning.
        }
        if head.starts_with("diff --git") {
            pending_header = true;
        }
        if consumed.len() >= PEEK_CAP {
            return Ok((false, consumed)); // too much preamble: treat as non-diff.
        }
    }
}

/// The ANSI-stripped first 64 chars of a line — enough to see any git header
/// marker without running the stripper over a pathologically long line.
fn strip_head(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let head: String = text.chars().take(64).collect();
    uncurses::ansi::strip::strip(&head)
}

/// Whether `head` (already ANSI-stripped) begins a line git only emits inside a
/// real `diff --git` block, used to confirm a candidate header.
fn is_diff_continuation(head: &str) -> bool {
    const MARKERS: [&str; 11] = [
        "index ", "--- ", "+++ ", "@@", "old mode ", "new mode ", "new file mode ",
        "deleted file mode ", "similarity index ", "rename ", "copy ",
    ];
    head.starts_with("Binary files")
        || head.starts_with("GIT binary patch")
        || MARKERS.iter().any(|m| head.starts_with(m))
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

    /// The single-commit (`Source::Rev`) path renders a patch for both a child
    /// commit (diffed against its parent) and a root commit with no parent —
    /// the parentless case is the easy one to break.
    #[test]
    fn rev_single_commit_renders_patch() {
        let dir = std::env::temp_dir().join(format!("drift-rev-{}", std::process::id()));
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
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "root"]);
        let root = String::from_utf8(run(&["rev-parse", "HEAD"]).stdout).unwrap().trim().to_string();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        run(&["commit", "-qam", "child"]);
        let child = String::from_utf8(run(&["rev-parse", "HEAD"]).stdout).unwrap().trim().to_string();

        let diff_of = |rev: &str| {
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(&dir).unwrap();
            let out = Source::Rev(rev.to_string()).diff(&Opts::default());
            std::env::set_current_dir(prev).unwrap();
            out.unwrap()
        };

        // Root commit (no parent): its initial contents show as a new file.
        let root_diff = diff_of(&root);
        assert!(root_diff.contains("diff --git"), "root not a patch:\n{root_diff}");
        assert!(root_diff.contains("new file"), "root missing new-file header:\n{root_diff}");
        assert!(root_diff.contains("+one"), "root missing added line:\n{root_diff}");

        // Child commit: diffed against its parent.
        let child_diff = diff_of(&child);
        assert!(child_diff.contains("-one"), "child missing removal:\n{child_diff}");
        assert!(child_diff.contains("+two"), "child missing addition:\n{child_diff}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `peek_diff` recognizes a real unified diff (even colorized), rejects
    /// non-diff output like `git diff --stat`, preserves bytes exactly for
    /// pass-through (including non-UTF-8), and stops right after the header it
    /// confirms so the streamer resumes from the next byte.
    #[test]
    fn peek_diff_detects_diff_vs_non_diff() {
        use std::io::Read;

        // A unified diff: detected once the `@@` confirms the header; the prefix
        // carries the header *and* the confirming line, byte-for-byte.
        let diff = b"diff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n";
        let (is, pre) = peek_diff(&diff[..]).unwrap();
        assert!(is);
        assert_eq!(pre.as_slice(), &b"diff --git a/f b/f\n@@ -1 +1 @@\n"[..]);

        // git show preamble before the diff: still detected; the prefix is the
        // preamble plus header+confirmation, and peek must NOT over-consume —
        // the streamer resumes from the very next byte of the shared stdin.
        let show = b"commit abc\nAuthor: t\n\n    msg\n\ndiff --git a/f b/f\n@@ -1 +1 @@\n-a\n+b\n";
        let mut rest = &show[..];
        let (is, pre) = peek_diff(&mut rest).unwrap();
        assert!(is);
        assert!(pre.starts_with(b"commit abc"));
        assert!(pre.ends_with(b"@@ -1 +1 @@\n"));
        let mut tail = Vec::new();
        rest.read_to_end(&mut tail).unwrap();
        assert_eq!(tail.as_slice(), &b"-a\n+b\n"[..], "peek must stop after the confirmation line");

        // Non-diff (--stat): not detected; all bytes consumed for pass-through.
        let stat = b" f | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n";
        let (is, pre) = peek_diff(&stat[..]).unwrap();
        assert!(!is);
        assert_eq!(pre.as_slice(), &stat[..]);

        // Non-UTF-8 bytes must pass through byte-for-byte, not abort drift.
        let bin = b"plain\xff\xfe stat bytes\n";
        let (is, pre) = peek_diff(&bin[..]).unwrap();
        assert!(!is, "invalid UTF-8 must not be misclassified");
        assert_eq!(pre.as_slice(), &bin[..], "non-UTF-8 bytes must survive pass-through");

        // Ordinary prose that merely contains a `diff --git ...` line is NOT a
        // diff (no git continuation line follows) — it passes through untouched.
        let prose = b"ordinary text\ndiff --git not actually a patch\nmore text\n";
        let (is, pre) = peek_diff(&prose[..]).unwrap();
        assert!(!is, "prose containing `diff --git` must not open the TUI");
        assert_eq!(pre.as_slice(), &prose[..]);

        // Combined merge diff (`diff --cc`) is passed through: the parser only
        // splits on `diff --git`, so this is a deliberate decision, not a bug.
        let cc = b"diff --cc f\n@@@ -1,1 -1,1 +1,1 @@@\n";
        let (is, _) = peek_diff(&cc[..]).unwrap();
        assert!(!is);

        // Colorized `diff --git` (git's pager coloring), confirmed by `index`:
        // detected, and the consumed bytes keep their ANSI codes.
        let colored = b"\x1b[1mdiff --git a/f b/f\x1b[m\nindex 00..11 100644\n";
        let (is, pre) = peek_diff(&colored[..]).unwrap();
        assert!(is);
        assert_eq!(pre.as_slice(), &colored[..], "ANSI codes must survive in the prefix");

        // Non-diff with ANSI (colorized `--stat`): rejected, every byte — escape
        // codes included — preserved so pass-through keeps color.
        let colored_stat = b" f | 2 \x1b[32m+\x1b[m\x1b[31m-\x1b[m\n";
        let (is, pre) = peek_diff(&colored_stat[..]).unwrap();
        assert!(!is);
        assert_eq!(pre.as_slice(), &colored_stat[..], "ANSI codes must survive pass-through");

        // Empty stream: not a diff, nothing consumed.
        let (is, pre) = peek_diff(&b""[..]).unwrap();
        assert!(!is);
        assert!(pre.is_empty());

        // Valid multibyte UTF-8 (CJK) non-diff output — e.g. a localized
        // `--stat` summary under a non-English locale — passes through
        // byte-for-byte and is not misclassified as a diff.
        let jp = "ファイル | 2 +-\n 1 個のファイルが変更されました\n".as_bytes();
        let (is, pre) = peek_diff(jp).unwrap();
        assert!(!is);
        assert_eq!(pre.as_slice(), jp);

        // A real diff with CJK paths/content and a localized (CJK) commit
        // preamble is still detected: the header tokens git emits (`diff --git`,
        // `index`, `@@`) are plumbing and never localized, so classification is
        // charset- and locale-independent.
        let cjk_diff =
            "コミット abc\n作者: 田中\n\ndiff --git a/文書.txt b/文書.txt\nindex 1..2 100644\n@@ -1 +1 @@\n-你好世界\n+你好宇宙\n"
                .as_bytes();
        let (is, pre) = peek_diff(cjk_diff).unwrap();
        assert!(is);
        assert!(pre.ends_with("index 1..2 100644\n".as_bytes()));

        // A long non-diff stream stops at the cap instead of buffering it all.
        let mut big = Vec::new();
        while big.len() < PEEK_CAP + 4096 {
            big.extend_from_slice(b"just some non-diff output line\n");
        }
        let total = big.len();
        let (is, pre) = peek_diff(&big[..]).unwrap();
        assert!(!is);
        assert!(pre.len() >= PEEK_CAP && pre.len() < total, "peek must stop at the cap");
    }
}
