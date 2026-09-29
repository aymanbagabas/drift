//! Shell completion, driven through the `DRIFT_COMPLETE=<shell>` protocol
//! that the registered shell scripts use.

use std::path::Path;
use std::process::Command;

/// Candidates for `words` under the protocol variables in `env`. The shell
/// runs drift as `drift -- drift <words>`.
fn complete(dir: &Path, env: &[(&str, &str)], words: &[&str]) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_drift"))
        .current_dir(dir)
        .envs(env.iter().copied())
        .args(["--", "drift"])
        .args(words)
        .output()
        .unwrap();
    // fish follows each candidate with a tab and its help text.
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('\t').next().unwrap().to_string())
        .collect()
}

#[test]
fn completes_flags_values_paths_and_revisions() {
    let dir = std::env::temp_dir().join(format!("drift-complete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dir")).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git").current_dir(&dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t.co"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
    std::fs::write(dir.join("dir/nested.txt"), "one\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
    git(&["commit", "-qam", "second"]);
    git(&["branch", "feature"]);
    git(&["tag", "v1"]);
    // Check out a branch that is then deleted, then `feature`: `@{-1}` is
    // `feature`, and `@{-3}` names the deleted branch, which can't resolve.
    git(&["checkout", "-q", "-b", "gone"]);
    git(&["checkout", "-q", "-"]);
    git(&["branch", "-q", "-D", "gone"]);
    git(&["checkout", "-q", "feature"]);
    git(&["checkout", "-q", "-"]);
    git(&["update-ref", "ORIG_HEAD", "HEAD~1"]);
    git(&["branch", "café"]);
    let head = git(&["rev-parse", "HEAD"]);
    // fish passes the words up to the cursor, so the last one is under it.
    let fish = |words: &[&str]| complete(&dir, &[("DRIFT_COMPLETE", "fish")], words);

    // The revision: refs, the pseudo-refs that exist, plus the flags.
    let first = fish(&[""]);
    for want in ["HEAD", "ORIG_HEAD", "feature", "v1", "--staged"] {
        assert!(first.iter().any(|c| c == want), "no {want} in {first:?}");
    }
    for unwanted in ["FETCH_HEAD", "tracked.txt"] {
        assert!(!first.iter().any(|c| c == unwanted), "{unwanted} in {first:?}");
    }
    assert_eq!(fish(&["fe"]), ["feature"]);
    assert_eq!(fish(&["HEAD..fe"]), ["HEAD..feature"]);
    assert_eq!(fish(&["HEAD...v"]), ["HEAD...v1"]);
    assert_eq!(fish(&["refs/t"]), ["refs/tags/v1"]);

    // Commits by hash, ancestors, and reflog entries.
    let by_hash = fish(&[&head[..6]]);
    assert!(by_hash.len() == 1 && head.starts_with(&by_hash[0]), "{by_hash:?}");
    // More digits than git's abbreviation still name the commit.
    let by_long_hash = fish(&[&head[..16]]);
    assert!(by_long_hash.len() == 1 && head.starts_with(&by_long_hash[0]), "{by_long_hash:?}");
    assert_eq!(fish(&["HEAD~"]), ["HEAD~1"]);
    assert!(fish(&["HEAD@{"]).iter().any(|c| c == "HEAD@{1}"));
    let prior = fish(&["@{"]);
    assert!(prior.iter().any(|c| c == "@{-1}"), "{prior:?}");
    assert!(!prior.iter().any(|c| c == "@{-3}"), "{prior:?}");

    // Paths in a revision's tree, also on both sides of a range.
    assert_eq!(fish(&["HEAD:"]), ["HEAD:dir/", "HEAD:tracked.txt"]);
    assert_eq!(fish(&["HEAD:dir/"]), ["HEAD:dir/nested.txt"]);
    assert_eq!(fish(&["HEAD~1:tr..HEAD:tr"]), ["HEAD~1:tr..HEAD:tracked.txt"]);

    assert_eq!(fish(&["--diff-algorithm", "p"]), ["patience"]);

    // Paths go after `--`, with or without a revision before it.
    assert_eq!(fish(&["--", "tr"]), ["tracked.txt"]);
    assert_eq!(fish(&["feature", "--", "tr"]), ["tracked.txt"]);
    // Without `--`, clap rejects a second positional, so offer only flags.
    let second = fish(&["feature", ""]);
    assert!(second.iter().all(|c| c.starts_with('-')), "{second:?}");

    // zsh passes every word plus the cursor word's index. A `--` after the
    // cursor must not turn the revision under it into a path.
    let zsh = [("DRIFT_COMPLETE", "zsh"), ("_CLAP_COMPLETE_INDEX", "1")];
    assert_eq!(complete(&dir, &zsh, &["fe", "--", "tracked.txt"]), ["feature"]);

    // After `-C DIR` or `--directory DIR`, revisions and paths come from DIR,
    // where drift will run.
    let other = std::env::temp_dir().join(format!("drift-complete-other-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&other);
    std::fs::create_dir_all(&other).unwrap();
    let other_git = |args: &[&str]| {
        assert!(Command::new("git").current_dir(&other).args(args).status().unwrap().success());
    };
    other_git(&["init", "-q"]);
    other_git(&["-c", "user.email=t@t.co", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
    other_git(&["branch", "elsewhere"]);
    std::fs::write(other.join("other.txt"), "").unwrap();
    let other_dir = other.to_str().unwrap();
    assert_eq!(fish(&["-C", other_dir, "else"]), ["elsewhere"]);
    assert_eq!(fish(&["--directory", other_dir, "--", "oth"]), ["other.txt"]);
    let _ = std::fs::remove_dir_all(&other);
    // drift fails if it can't enter DIR, so nothing completes. The words
    // must not come from the directory drift started in.
    let missing = dir.join("missing");
    let missing = missing.to_str().unwrap();
    for words in [&["-C", missing, "fe"][..], &["--directory", missing, "--", "tr"]] {
        let got = fish(words);
        assert!(got.is_empty(), "{words:?}: {got:?}");
    }

    // Only `DRIFT_COMPLETE` starts completion, not an unrelated `COMPLETE`.
    let out = Command::new(env!("CARGO_BIN_EXE_drift")).env("COMPLETE", "yes").arg("--version").output().unwrap();
    assert!(out.status.success() && out.stdout.starts_with(b"drift "), "{out:?}");

    // bash breaks a word at `:` and `@`, and replaces only the part after
    // the last one, so drift's bash script must hand back just that part. It
    // must also keep bash from expanding `dir/[ab].txt` into `dir/a.txt`.
    // `/bin/bash` is bash 3.2 on macOS.
    #[cfg(unix)]
    {
        std::fs::write(dir.join("dir/[ab].txt"), "").unwrap();
        std::fs::write(dir.join("dir/a.txt"), "").unwrap();
        for bash in ["bash", "/bin/bash"].into_iter().filter(|b| *b == "bash" || Path::new(b).exists()) {
            for (line, word, want) in [
                ("drift HEAD:", "", "dir/"),
                ("drift HEAD~1:tr..HEAD:tr", "tr", "tracked.txt"),
                ("drift HEAD@{", "@{", "@{1}"),
                ("drift --diff-algorithm=p", "p", "patience"),
                ("drift -- dir/[", "dir/[", "dir/[ab].txt"),
            ] {
                let bin = Path::new(env!("CARGO_BIN_EXE_drift")).parent().unwrap();
                let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
                let script = r#"eval "$(drift --completions bash)"
                    COMP_LINE=$LINE COMP_POINT=${#LINE} _drift drift "$WORD"
                    printf '%s\n' "${COMPREPLY[@]}""#;
                let out = Command::new(bash)
                    .current_dir(&dir)
                    .env("PATH", path)
                    .env("LINE", line)
                    .env("WORD", word)
                    .args(["-c", script])
                    .output()
                    .unwrap();
                let got = String::from_utf8(out.stdout).unwrap();
                assert!(got.lines().any(|c| c == want), "{bash} {line}: no {want} in {got:?}");
            }
        }
    }

    // PowerShell runs the typed line again as code, so drift's script must
    // take the words from the parsed command and quote a `@{` value.
    // `powershell` is Windows PowerShell 5.1, which drops an empty argument.
    // The last case sets a console code page other than UTF-8, as Windows
    // has, where drift's output must still read as UTF-8. The script starts
    // with a byte order mark, or Windows PowerShell misreads `café`.
    let ps1 = std::env::temp_dir().join(format!("drift-complete-{}.ps1", std::process::id()));
    std::fs::write(
        &ps1,
        concat!("\u{feff}", r#"drift --completions powershell | Out-String | Invoke-Expression
foreach ($line in @('drift ', 'drift HEAD:', "drift 'HEAD@{", 'drift -- tr', 'drift --diff-algorithm=p')) {
    (TabExpansion2 $line $line.Length).CompletionMatches.CompletionText -join ' '
}
try { [Console]::OutputEncoding = [Text.Encoding]::GetEncoding(28591) } catch {}
$before = [Console]::OutputEncoding.CodePage
$found = (TabExpansion2 'drift caf' 9).CompletionMatches.CompletionText
"$($found -contains 'café') $([Console]::OutputEncoding.CodePage -eq $before)"
"#),
    )
    .unwrap();
    let bin = Path::new(env!("CARGO_BIN_EXE_drift")).parent().unwrap().to_path_buf();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    for ps in ["pwsh", "powershell"] {
        let Ok(out) = Command::new(ps)
            .current_dir(&dir)
            .env("PATH", &path)
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&ps1)
            .output()
        else {
            continue; // Not installed.
        };
        let got = String::from_utf8_lossy(&out.stdout);
        let err = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<Vec<&str>> = got.lines().map(|l| l.split(' ').collect()).collect();
        assert_eq!(lines.len(), 6, "{ps}: {got}{err}");
        assert!(lines[0].contains(&"HEAD"), "{ps}: {got}{err}");
        assert!(lines[1].contains(&"HEAD:dir/"), "{ps}: {got}{err}");
        assert!(lines[2].contains(&"'HEAD@{1}'"), "{ps}: {got}{err}");
        assert_eq!(lines[3], ["tracked.txt"], "{ps}: {got}{err}");
        assert_eq!(lines[4], ["--diff-algorithm=patience"], "{ps}: {got}{err}");
        assert_eq!(lines[5], ["True", "True"], "{ps}: {got}{err}");
    }
    let _ = std::fs::remove_file(&ps1);

    let _ = std::fs::remove_dir_all(&dir);
}
