//! drift — a standalone git diff pager for the terminal.

mod config;
mod diff;
mod git;
mod highlight;
mod tui;
mod watch;

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::builder::PossibleValue;
use clap::{CommandFactory, Parser, ValueHint};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter as _};
use clap_complete::env::Shells;
use clap_complete::{CompleteEnv, Shell};

use config::Config;
use git::Source;

// mimalloc cuts CPU time and peak memory when highlighting large diffs, most of
// all on the static musl release binary. Off on Windows only: its C sources do
// not cross-compile under the windows-gnu (zig) release toolchain (see Cargo.toml).
#[cfg(not(target_os = "windows"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// A standalone git diff pager: browse commit, staged, or working-tree diffs
/// in a TUI with syntax highlighting, intra-line changes, and live refresh.
#[derive(Parser, Debug)]
#[command(name = "drift", version, about)]
struct Cli {
    /// Commit to show, or a revision range like `main..feature`.
    #[arg(add = ArgValueCompleter::new(complete_revision))]
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

    /// Run as if drift was started in this directory (like `git -C`). Works
    /// with worktrees and bare repositories.
    #[arg(short = 'C', long = "directory", value_name = "DIR", value_hint = ValueHint::DirPath)]
    directory: Option<PathBuf>,

    /// Path to a config file (toml/yaml/json). Overrides auto-discovery.
    #[arg(short, long, value_hint = ValueHint::FilePath)]
    config: Option<PathBuf>,

    /// Disable syntax highlighting for this run.
    #[arg(long)]
    no_syntax: bool,

    /// Collapse the commit metadata to just the commit line by default (the
    /// author, date, and message stay hidden until you expand it — `enter` or
    /// a double-click on the commit line).
    #[arg(long = "no-commit-meta")]
    no_commit_meta: bool,

    /// Wrap long lines instead of scrolling horizontally. Toggle at runtime
    /// with `w`.
    #[arg(long)]
    wrap: bool,

    /// Force wrapping off for this run (overrides the config default).
    #[arg(long = "no-wrap", conflicts_with = "wrap")]
    no_wrap: bool,

    /// Ignore whitespace-only changes (git `-w`).
    #[arg(long = "ignore-whitespace")]
    ignore_whitespace: bool,

    /// Lines of context around each change (git `-U`).
    #[arg(short = 'U', long = "context", value_name = "N")]
    context: Option<usize>,

    /// Diff algorithm (git `--diff-algorithm`).
    // git also takes `default` for myers, in any case.
    #[arg(
        long = "diff-algorithm",
        value_name = "ALGO",
        ignore_case = true,
        value_parser = [
            PossibleValue::new("myers").alias("default"),
            PossibleValue::new("minimal"),
            PossibleValue::new("patience"),
            PossibleValue::new("histogram"),
        ]
    )]
    diff_algorithm: Option<String>,

    /// Limit the diff to these paths (after `--`), e.g. `drift -- src/ docs/`.
    #[arg(last = true, value_name = "PATHSPEC", add = ArgValueCompleter::new(complete_pathspec))]
    pathspec: Vec<String>,

    /// Print the script that sets up completion for SHELL, then exit.
    #[arg(long, value_name = "SHELL", exclusive = true)]
    completions: Option<Shell>,
}

/// Complete a git revision (see git::complete_revision), including the right
/// side of an `a..b` or `a...b` range.
fn complete_revision(current: &OsStr) -> Vec<CompletionCandidate> {
    if after_escape() {
        return complete_pathspec(current);
    }
    if !enter_directory() {
        return Vec::new();
    }
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let (range, rev) = current.rfind("..").map_or(("", current), |i| current.split_at(i + 2));
    git::complete_revision(rev)
        .into_iter()
        .map(|(value, help)| CompletionCandidate::new(format!("{range}{value}")).help(help.map(Into::into)))
        .collect()
}

/// Complete paths, but only after `--`, the one place clap takes a pathspec.
fn complete_pathspec(current: &OsStr) -> Vec<CompletionCandidate> {
    if !after_escape() || !enter_directory() {
        return Vec::new();
    }
    PathCompleter::any().complete(current)
}

/// The words before the one under the cursor, starting with the command name.
/// `CompleteEnv` runs drift as `drift -- <words>`. Some shells' scripts give
/// the cursor word's index in `_CLAP_COMPLETE_INDEX`; the others end the
/// words at the cursor word.
fn words_before_cursor() -> Vec<OsString> {
    let mut words: Vec<OsString> = std::env::args_os().skip(2).collect();
    let cursor = std::env::var("_CLAP_COMPLETE_INDEX").ok().and_then(|i| i.parse().ok());
    words.truncate(cursor.unwrap_or(words.len().saturating_sub(1)));
    words
}

/// Whether a `--` comes before the word being completed. clap_complete's
/// engine ignores `last = true`: it offers the revision after `--` and the
/// pathspec before it, so the two completers above check for themselves.
fn after_escape() -> bool {
    words_before_cursor().iter().any(|w| w == "--")
}

/// Move into the directory that a `-C DIR` or `--directory DIR` before the
/// cursor names, since drift runs there. This process completes only the word
/// under the cursor, so the move affects nothing else. Returns false if the
/// move fails. drift then fails too, so the completers offer nothing.
fn enter_directory() -> bool {
    let words = words_before_cursor();
    let mut dir = None;
    let mut words = words.iter().skip(1).take_while(|w| *w != "--");
    while let Some(word) = words.next() {
        if word == "-C" || word == "--directory" {
            dir = words.next().map(PathBuf::from);
        } else if let Some(d) = word.to_str().and_then(|w| w.strip_prefix("--directory=").or(w.strip_prefix("-C"))) {
            dir = Some(PathBuf::from(d));
        }
    }
    let Some(dir) = dir else {
        return true;
    };
    // The shell expands a leading `~` only when it runs the command.
    let dir = match (dir.strip_prefix("~"), std::env::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => dir,
    };
    std::env::set_current_dir(dir).is_ok()
}

/// Print the script that sets up completion for `shell`. On each TAB, the
/// script runs `DRIFT_COMPLETE=<shell> drift -- <words>`, which main handles.
/// It runs `drift` from PATH, so a package can ship the script.
///
/// bash, zsh, fish, and PowerShell get drift's own scripts below, because
/// clap_complete's scripts break revision words such as `HEAD:src/` and
/// `HEAD@{1}` (clap-rs/clap#6280 and more). When clap_complete's own scripts
/// handle those words, print its scripts for every shell, delete drift's, and
/// remove the exact clap_complete version pin in Cargo.toml.
fn print_completions(shell: Shell) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    match shell {
        Shell::Bash => out.write_all(BASH_COMPLETION.as_bytes()),
        Shell::Zsh => out.write_all(ZSH_COMPLETION.as_bytes()),
        Shell::Fish => out.write_all(FISH_COMPLETION.as_bytes()),
        Shell::PowerShell => out.write_all(POWERSHELL_COMPLETION.as_bytes()),
        _ => {
            let shells = Shells::builtins();
            let completer = shells.completer(&shell.to_string()).expect("a completer for each shell");
            completer.write_registration(COMPLETE_VAR, "drift", "drift", "drift", &mut out)
        }
    }
}

/// The variable that puts drift in completion mode. It is not clap's default
/// `COMPLETE`: that name is so generic that an unrelated `COMPLETE` in the
/// environment would stop drift from running. The scripts below spell it out.
const COMPLETE_VAR: &str = "DRIFT_COMPLETE";

/// drift's bash script, used instead of clap's. clap's script takes the words
/// from COMP_WORDS, which bash breaks at `:` and `@`, so a revision like
/// `HEAD:src/` or `HEAD@{1}` reaches drift in pieces. This script splits the
/// line up to the cursor on whitespace, then trims each candidate to the part
/// of the word that bash replaces. drift separates the candidates with a
/// vertical tab, since a path can hold a space, and `read` splits them, since
/// an unquoted `$(...)` would also expand a path like `[id].tsx` as a pattern.
const BASH_COMPLETION: &str = r#"_drift() {
    local IFS=$' \t\n' words cur
    read -ra words <<< "${COMP_LINE:0:COMP_POINT}"
    [[ ${COMP_LINE:COMP_POINT-1:1} == [[:space:]] ]] && words+=("")
    cur=${words[${#words[@]}-1]}
    IFS=$'\013' read -ra COMPREPLY <<< "$(_CLAP_IFS=$'\013' _CLAP_COMPLETE_INDEX=$((${#words[@]}-1)) DRIFT_COMPLETE=bash drift -- "${words[@]}" 2>/dev/null)"
    COMPREPLY=("${COMPREPLY[@]#"${cur%"$2"}"}")
    if compopt +o nospace 2>/dev/null && [[ ${COMPREPLY-} =~ [=/:]$ ]]; then
        compopt -o nospace
    fi
}
complete -o nospace -o bashdefault -o nosort -F _drift drift 2>/dev/null ||
    complete -o nospace -o bashdefault -F _drift drift
"#;

/// drift's zsh script, used instead of clap's. clap's script passes the words
/// with their shell quoting still on. It also takes any value with a `:` for a
/// file, so `HEAD:src/` gets a trailing space. And it only registers itself,
/// so when zsh autoloads it from the fpath, the first TAB completes nothing.
/// This script unquotes the words, ends them with the unquoted text before the
/// cursor (zsh takes the quotes off `$PREFIX` but keeps its backslashes), and
/// asks drift in fish's protocol: the last word is the one under the cursor,
/// and each candidate comes back as `value<TAB>help`.
const ZSH_COMPLETION: &str = r#"#compdef drift
_drift() {
    local -a dirs others
    local line value desc ret=1
    # zsh drops an unmatched `{` from the word and puts it back after the
    # match it inserts, which would garble `HEAD@{1}`. `HEAD@\{` and
    # `'HEAD@{` complete.
    if [[ $PREFIX$SUFFIX != *\{* && ${(Q)words[CURRENT]} == *\{* ]]; then
        _message 'reflog entry (quote the word, or escape the brace: @\{)'
        return 1
    fi
    for line in "${(@f)$(DRIFT_COMPLETE=fish drift -- "${(@Q)words[1,CURRENT-1]}" "${(Q)PREFIX}" 2>/dev/null)}"; do
        [[ -n $line ]] || continue
        value=${line%%$'\t'*} desc=${line#*$'\t'}
        [[ $desc == "$line" ]] && desc=
        value=${${value//\\/\\\\}//:/\\:}
        if [[ $value == */ ]]; then
            dirs+=("${value%/}${desc:+:$desc}")
        else
            others+=("$value${desc:+:$desc}")
        fi
    done
    # Return 0 for any match, or zsh also runs its next completer, such as
    # _approximate, which runs drift again.
    _describe -V values dirs -S / -r / && ret=0
    _describe -V values others && ret=0
    return ret
}
if [[ $funcstack[1] == _drift ]]; then _drift "$@"; else compdef _drift drift; fi
"#;

/// drift's fish script, used instead of clap's. clap's script passes the word
/// under the cursor with its quoting still on, so `'HEAD:src/m` and `HEAD@\{`
/// match nothing. This script unquotes it.
const FISH_COMPLETION: &str = r#"function __drift_complete
    set -l token (commandline --current-token)
    # After an unescaped `{`, fish would insert the rest of `HEAD@{1}` escaped,
    # as `1\}`, and leave the brace open. `HEAD@\{` and `'HEAD@{` complete.
    string match -qr -- '^[^\'"\\\\]*\\{' "$token"; and return
    DRIFT_COMPLETE=fish drift -- (commandline --current-process --tokenize --cut-at-cursor) (string unescape -- "$token")
end
complete --keep-order --exclusive --command drift --arguments '(__drift_complete)'
"#;

/// drift's PowerShell script, used instead of clap's. clap's script runs the
/// typed line again as PowerShell code, so a quoted word such as `'HEAD@{`
/// fails to parse and completes nothing. It also inserts a value such as
/// `HEAD@{1}` without quotes, and PowerShell then passes `HEAD@` and a script
/// block. This script takes the words from the parsed command and quotes such
/// a value. It asks drift in zsh's protocol, where drift adds the empty word
/// under the cursor itself: Windows PowerShell drops an empty argument. It
/// reads drift's output as UTF-8: PowerShell decodes it in the console's code
/// page, which on Windows turns `café` into `cafÃ©`.
const POWERSHELL_COMPLETION: &str = r#"Register-ArgumentCompleter -Native -CommandName drift -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $words = @(foreach ($e in $commandAst.CommandElements) {
        if ($e.Extent.StartOffset -ge $cursorPosition) { break }
        if ($e -is [System.Management.Automation.Language.StringConstantExpressionAst]) { $e.Value } else { $e.Extent.Text }
    })
    $encoding = [Console]::OutputEncoding
    try {
        [Console]::OutputEncoding = [Text.UTF8Encoding]::new()
        $env:DRIFT_COMPLETE = 'zsh'
        $env:_CLAP_COMPLETE_INDEX = if ($wordToComplete) { $words.Count - 1 } else { $words.Count }
        $lines = & drift -- @words 2>$null
    } finally {
        [Console]::OutputEncoding = $encoding
        $env:DRIFT_COMPLETE = $null
        $env:_CLAP_COMPLETE_INDEX = $null
    }
    foreach ($line in $lines) {
        # value[:help], with each `\` and `:` in the value escaped by a `\`.
        if ($line -match '^((?:\\.|[^\\:])*)(?::(.*))?$') {
            $value = $Matches[1] -replace '\\(.)', '$1'
            $help = if ($Matches[2]) { $Matches[2] -replace '\\(.)', '$1' } else { $value }
            $text = if ($value -match '[\s''"`$@{}();,|&<>#]') { "'" + ($value -replace "'", "''") + "'" } else { $value }
            [System.Management.Automation.CompletionResult]::new($text, $value, 'ParameterValue', $help)
        }
    }
}
"#;

fn main() {
    // With `DRIFT_COMPLETE=<shell>` set, print the candidates for the words
    // the shell passes back (the `--completions` scripts do this on each
    // TAB), then exit.
    CompleteEnv::with_factory(Cli::command).var(COMPLETE_VAR).complete();

    if let Err(e) = run() {
        eprintln!("drift: {e}");
        std::process::exit(1);
    }
}

fn run() -> std::io::Result<()> {
    let cli = Cli::parse();
    if let Some(shell) = cli.completions {
        return print_completions(shell);
    }

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
    if cli.no_commit_meta {
        cfg.commit_meta = false;
    }
    if cli.wrap {
        cfg.wrap = true;
    }
    if cli.no_wrap {
        cfg.wrap = false;
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

    let Some(mut app) = tui::App::new(cfg, source, opts)? else {
        return Ok(()); // stdin wasn't a diff: peek printed it and bailed.
    };
    let result = app.run(refresh, cli.watch, Duration::from_millis(cli.poll_interval));
    app.finish()?;
    result
}
