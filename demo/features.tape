# drift feature showcase — render with:  vhs demo/features.tape
# Run from the repo root (needs target/release/drift built).
#
# Tours the diff features in watch mode. A detached background job commits the
# changes partway through; watch mode notices the now-clean tree, reloads, and
# the idle mascot 👾 takes over the screen and drifts free to the end.
Output demo/drift.gif

Set Shell "bash"
Set FontSize 15
Set Width 1300
Set Height 820
Set Padding 18
Set Theme "Dracula"
Set TypingSpeed 55ms

# --- hidden setup: build the demo repo, alias drift to the release binary ---
Hide
Type 'alias drift="$PWD/target/release/drift"' Enter
Type "sh demo/setup.sh >/dev/null 2>&1 && cd /tmp/drift-demo" Enter
Type "clear" Enter
# Detached commit fires ~27s in — after the feature tour below. Watch mode
# then reloads to a clean tree and hands the screen to the mascot.
Type "( sleep 27 && git add -A && git commit -qm 'ship it' ) >/dev/null 2>&1 &" Enter
Type "clear" Enter
Show

# --- working tree vs HEAD, untracked files, watch mode --------------------
Type "drift -w -A" Sleep 500ms Enter
Sleep 2.5s

# scroll through the first file's diff
Down@120ms 8
Sleep 800ms
Type "}" Sleep 700ms
Type "}" Sleep 900ms

# walk between files — the sidebar highlight follows each one
Type "]" Sleep 1.4s
Type "]" Sleep 1.4s
Type "[" Sleep 1.4s

# split (side-by-side) view
Type "s" Sleep 2.5s
Type "s" Sleep 800ms

# toggle the file sidebar off and back on
Type "B" Sleep 1.4s
Type "B" Sleep 800ms

# file-list modal: pick a different file and jump to it
Type "F" Sleep 1.2s
Up@300ms 1
Sleep 900ms
Enter Sleep 1.6s

# in-file search with match highlighting
Type "/" Sleep 500ms
Type "log" Sleep 700ms Enter
Type "n" Sleep 700ms
Type "n" Sleep 900ms
Escape Sleep 800ms

# --- the commit lands: watch reloads, the mascot drifts free --------------
Sleep 10s

Hide
Type "q"
Sleep 500ms
