//! Git-centric file watching: notifies on staging (index) and branch/commit
//! (HEAD, refs) changes so the diff view can refresh. Uses the resolved
//! per-worktree git dir and shared common dir, so linked worktrees and bare
//! repos are covered.

use std::sync::mpsc::{channel, Receiver};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::git::Repo;

/// Start watching the repo's git internals. Returns a receiver that gets a
/// unit message on any relevant change, plus the watcher (keep it alive).
pub fn watch(repo: &Repo) -> notify::Result<(Receiver<()>, RecommendedWatcher)> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            // Coalescing happens on the consumer side; just signal.
            let _ = tx.send(());
        }
    })?;

    // Per-worktree dir: index (staging) and HEAD live here. Non-recursive is
    // enough and avoids churn from object writes.
    let _ = watcher.watch(&repo.git_dir, RecursiveMode::NonRecursive);

    // Shared refs: branch creation/switch/commit updates. packed-refs and
    // logs/HEAD sit in the common dir too.
    for sub in ["refs", "logs"] {
        let p = repo.common_dir.join(sub);
        if p.exists() {
            let _ = watcher.watch(&p, RecursiveMode::Recursive);
        }
    }
    if repo.common_dir != repo.git_dir {
        let _ = watcher.watch(&repo.common_dir, RecursiveMode::NonRecursive);
    }

    Ok((rx, watcher))
}
