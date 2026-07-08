//! diffv — a standalone git diff pager for the terminal.

mod config;
mod diff;
mod git;
mod highlight;
mod tui;
mod watch;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use config::Config;
use git::Source;

/// A standalone git diff pager: browse commit, staged, or working-tree diffs
/// in a TUI with syntax highlighting, intra-line changes, and live refresh.
#[derive(Parser, Debug)]
#[command(name = "diffv", version, about)]
struct Cli {
    /// Commit to show, or a revision range like `main..feature`.
    revision: Option<String>,

    /// Show staged changes (index vs HEAD).
    #[arg(long, visible_alias = "cached")]
    staged: bool,

    /// Also show untracked (non-ignored) files as new-file diffs. Only affects
    /// the working-tree view.
    #[arg(short = 'A', long = "all")]
    all: bool,

    /// Watch the git index and refs and refresh the diff on change.
    #[arg(short, long)]
    watch: bool,

    /// How often (ms) watch mode polls for unstaged working-tree edits, which
    /// the git index/refs watcher can't see.
    #[arg(long = "interval", value_name = "MS", default_value_t = 300)]
    poll_interval: u64,

    /// Run as if diffv was started in this directory (like `git -C`). Works
    /// with worktrees and bare repositories.
    #[arg(short = 'C', long = "directory", value_name = "DIR")]
    directory: Option<PathBuf>,

    /// Path to a config file (toml/yaml/json). Overrides auto-discovery.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Disable syntax highlighting for this run.
    #[arg(long)]
    no_syntax: bool,

    /// Ignore whitespace-only changes (git `-w`).
    #[arg(long = "ignore-whitespace")]
    ignore_whitespace: bool,

    /// Lines of context around each change (git `-U`).
    #[arg(short = 'U', long = "context", value_name = "N")]
    context: Option<usize>,

    /// Diff algorithm: myers, minimal, patience, or histogram.
    #[arg(long = "diff-algorithm", value_name = "ALGO")]
    diff_algorithm: Option<String>,

    /// Limit the diff to these paths (after `--`), e.g. `diffv -- src/ docs/`.
    #[arg(last = true, value_name = "PATHSPEC")]
    pathspec: Vec<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("diffv: {e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let cli = Cli::parse();

    // Resolve a relative `-c` path against the *original* working directory
    // before `-C` changes it, so the two flags compose.
    let config_path = cli
        .config
        .as_deref()
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));

    // Change directory first so git, config discovery, and watching all
    // operate against the requested repository.
    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir)
            .map_err(|e| std::io::Error::other(format!("cannot enter {}: {e}", dir.display())))?;
    }

    let mut cfg = Config::load(config_path.as_deref());
    if cli.no_syntax {
        cfg.syntax = false;
    }

    // Pager mode: a diff piped on stdin, with no explicit git selection.
    let piped = !std::io::stdin().is_terminal();
    let explicit = cli.staged || cli.revision.is_some();

    let mut source = if piped && !explicit {
        Source::Stdin
    } else if cli.staged {
        Source::Staged
    } else if let Some(rev) = cli.revision.clone() {
        Source::Rev(rev)
    } else {
        Source::Worktree
    };

    let opts = git::Opts {
        ignore_whitespace: cli.ignore_whitespace,
        context: cli.context,
        algorithm: cli.diff_algorithm.clone(),
        pathspec: cli.pathspec.clone(),
        all: cli.all,
    };

    // Everything but Stdin needs a repository.
    let repo = git::discover();
    if !matches!(source, Source::Stdin) && repo.is_none() {
        return Err(std::io::Error::other("not a git repository"));
    }

    // A bare repo has no working tree; fall back to showing HEAD.
    if repo.as_ref().is_some_and(|r| r.is_bare)
        && matches!(source, Source::Worktree | Source::Staged)
    {
        source = Source::Rev("HEAD".into());
    }

    // Watching only makes sense against a live repo. Start the watcher for any
    // non-stdin source so watch mode can be toggled at runtime; `cli.watch`
    // just sets whether it reacts to begin with.
    let watcher_guard;
    let refresh = if !matches!(source, Source::Stdin) {
        match repo.as_ref().map(watch::watch) {
            Some(Ok((rx, w))) => {
                watcher_guard = Some(w);
                Some(rx)
            }
            _ => {
                watcher_guard = None;
                None
            }
        }
    } else {
        watcher_guard = None;
        None
    };
    let _ = &watcher_guard;

    let mut app = tui::App::new(cfg, source, opts)?;
    let result = app.run(refresh, cli.watch, Duration::from_millis(cli.poll_interval));
    app.finish()?;
    result
}
