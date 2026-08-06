# drift feature showcase — render with:  vhs demo/features.tape
# Run from the repo root (needs target/release/drift built).
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
Show

# --- working tree vs HEAD, with untracked files ---------------------------
Type "drift -A" Sleep 500ms Enter
Sleep 2.5s

# scroll through the diff
Down@120ms 8
Sleep 800ms
Type "}" Sleep 700ms
Type "}" Sleep 900ms

# jump to the next file
Type "]" Sleep 1.5s

# split (side-by-side) view
Type "s" Sleep 2.5s
Type "s" Sleep 800ms

# file sidebar
Type "B" Sleep 1.8s
Type "B" Sleep 600ms

# file-list modal
Type "F" Sleep 1.6s
Down@250ms 2
Sleep 800ms
Enter Sleep 1.2s

# in-file search with match highlighting
Type "/" Sleep 500ms
Type "log" Sleep 700ms Enter
Type "n" Sleep 700ms
Type "n" Sleep 900ms
Escape Sleep 600ms

# help footer
Type "?" Sleep 2s
Type "?" Sleep 600ms

Type "q"
Sleep 1s
