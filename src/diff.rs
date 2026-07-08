//! Unified git diff parser: splits a diff into per-file sections, hunks, and
//! typed lines, and computes word-level intra-line changes.
//!
//! Parsing git's unified-diff text is hand-rolled (that's a format scan, not a
//! diff), but the intra-line word diff uses `similar` for real LCS matching.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Remove,
}

/// A segment of a line, tagged as changed when intra-line diffing found it
/// altered relative to its counterpart.
#[derive(Debug, Clone)]
pub struct Segment {
    pub text: String,
    pub changed: bool,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub content: String,
    /// Line number in the old file (for Context/Remove).
    pub old_no: Option<usize>,
    /// Line number in the new file (for Context/Add).
    pub new_no: Option<usize>,
    /// Word-level segments; empty means "render `content` unstyled".
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
    pub is_binary: bool,
    /// Extra header notes (new file, deleted, rename, mode change).
    pub notes: Vec<String>,
}

impl FileDiff {
    /// Display path: prefer the new path unless the file was deleted.
    pub fn path(&self) -> &str {
        if self.new_path != "/dev/null" && !self.new_path.is_empty() {
            &self.new_path
        } else {
            &self.old_path
        }
    }

    /// (additions, deletions) counted across all hunks.
    pub fn stats(&self) -> (usize, usize) {
        let mut add = 0;
        let mut del = 0;
        for h in &self.hunks {
            for l in &h.lines {
                match l.kind {
                    LineKind::Add => add += 1,
                    LineKind::Remove => del += 1,
                    LineKind::Context => {}
                }
            }
        }
        (add, del)
    }
}

/// Total (files, additions, deletions) across a set of file diffs.
pub fn totals(files: &[FileDiff]) -> (usize, usize, usize) {
    let mut add = 0;
    let mut del = 0;
    for f in files {
        let (a, d) = f.stats();
        add += a;
        del += d;
    }
    (files.len(), add, del)
}

fn strip_prefix(p: &str) -> String {
    // Strip the prefix git adds before a path. Default is a/ or b/, but with
    // diff.mnemonicPrefix git uses i/ (index), w/ (worktree), c/ (commit), or
    // o/ (object) instead. Leave /dev/null and unprefixed paths alone.
    for pre in ["a/", "b/", "i/", "w/", "c/", "o/"] {
        if let Some(rest) = p.strip_prefix(pre) {
            return rest.to_string();
        }
    }
    p.to_string()
}

/// Split a raw unified diff into per-file sections, one string per `diff --git`
/// block, in the same order (and count) as [`parse`]. Any preamble before the
/// first block (e.g. `git show` commit headers) is dropped. Used to copy an
/// exact per-file patch without reconstructing it.
pub fn split_files(input: &str) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    for line in input.lines() {
        if line.starts_with("diff --git") {
            sections.push(String::new());
        }
        if let Some(cur) = sections.last_mut() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    sections
}

/// Parse a unified diff (as produced by `git diff` / `git show`) into files.
pub fn parse(input: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut lines = input.lines().peekable();

    while let Some(line) = lines.next() {
        if !line.starts_with("diff --git") {
            continue;
        }
        let mut file = FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: Vec::new(),
            is_binary: false,
            notes: Vec::new(),
        };
        if let Some((a, b)) = parse_diff_header(line) {
            file.old_path = a;
            file.new_path = b;
        }

        // Consume the extended header until the first hunk or next file.
        while let Some(&next) = lines.peek() {
            if next.starts_with("diff --git") || next.starts_with("@@") {
                break;
            }
            let h = lines.next().unwrap();
            if let Some(p) = h.strip_prefix("--- ") {
                file.old_path = strip_prefix(p.trim());
            } else if let Some(p) = h.strip_prefix("+++ ") {
                file.new_path = strip_prefix(p.trim());
            } else if h.starts_with("new file mode") {
                file.notes.push("new file".into());
            } else if h.starts_with("deleted file mode") {
                file.notes.push("deleted".into());
            } else if let Some(r) = h.strip_prefix("rename from ") {
                file.notes.push(format!("renamed from {r}"));
            } else if h.starts_with("Binary files") || h.contains("GIT binary patch") {
                file.is_binary = true;
            } else if h.starts_with("old mode") || h.starts_with("new mode") {
                file.notes.push(h.trim().to_string());
            }
        }

        // Parse hunks.
        while let Some(&next) = lines.peek() {
            if next.starts_with("diff --git") {
                break;
            }
            let hline = lines.next().unwrap();
            if !hline.starts_with("@@") {
                continue;
            }
            let (mut old_no, mut new_no) = parse_hunk_header(hline);
            let mut hunk = Hunk {
                header: hline.to_string(),
                lines: Vec::new(),
            };
            while let Some(&next) = lines.peek() {
                if next.starts_with("@@") || next.starts_with("diff --git") {
                    break;
                }
                let l = lines.next().unwrap();
                if l == "\\ No newline at end of file" {
                    continue;
                }
                let (kind, content) = match l.as_bytes().first() {
                    Some(b'+') => (LineKind::Add, &l[1..]),
                    Some(b'-') => (LineKind::Remove, &l[1..]),
                    Some(b' ') => (LineKind::Context, &l[1..]),
                    _ => (LineKind::Context, l),
                };
                let (o, n) = match kind {
                    LineKind::Add => {
                        let n = Some(new_no);
                        new_no += 1;
                        (None, n)
                    }
                    LineKind::Remove => {
                        let o = Some(old_no);
                        old_no += 1;
                        (o, None)
                    }
                    LineKind::Context => {
                        let o = Some(old_no);
                        let n = Some(new_no);
                        old_no += 1;
                        new_no += 1;
                        (o, n)
                    }
                };
                hunk.lines.push(DiffLine {
                    kind,
                    content: content.to_string(),
                    old_no: o,
                    new_no: n,
                    segments: Vec::new(),
                });
            }
            intraline(&mut hunk.lines);
            file.hunks.push(hunk);
        }
        files.push(file);
    }
    files
}

fn parse_diff_header(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    // Split the two paths at the second path's prefix. Default is " b/", but
    // diff.mnemonicPrefix uses " w/" or " i/" for the after-side.
    for sep in [" b/", " w/", " i/", " c/", " o/"] {
        if let Some(idx) = rest.find(sep) {
            let a = &rest[..idx];
            let b = &rest[idx + 1..];
            return Some((strip_prefix(a.trim()), strip_prefix(b.trim())));
        }
    }
    let mut it = rest.split_whitespace();
    Some((strip_prefix(it.next()?), strip_prefix(it.next()?)))
}

fn parse_hunk_header(line: &str) -> (usize, usize) {
    // @@ -old_start,old_len +new_start,new_len @@ heading
    let mut old_no = 1;
    let mut new_no = 1;
    for tok in line.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old_no = rest.split(',').next().and_then(|s| s.parse().ok()).unwrap_or(1);
        } else if let Some(rest) = tok.strip_prefix('+') {
            new_no = rest.split(',').next().and_then(|s| s.parse().ok()).unwrap_or(1);
        }
    }
    (old_no, new_no)
}

/// Compute word-level segments for balanced remove/add runs within a hunk.
/// Only paired lines (one removed matched to one added) get segments.
fn intraline(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind == LineKind::Remove {
            let rstart = i;
            let mut j = i;
            while j < lines.len() && lines[j].kind == LineKind::Remove {
                j += 1;
            }
            let astart = j;
            let mut k = j;
            while k < lines.len() && lines[k].kind == LineKind::Add {
                k += 1;
            }
            let rlen = astart - rstart;
            let alen = k - astart;
            if rlen == alen && rlen > 0 {
                for p in 0..rlen {
                    let old = lines[rstart + p].content.clone();
                    let new = lines[astart + p].content.clone();
                    let (oseg, nseg) = word_diff(&old, &new);
                    lines[rstart + p].segments = oseg;
                    lines[astart + p].segments = nseg;
                }
            }
            i = k.max(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Intra-line diff using `similar`'s word-level LCS: shared words stay in the
/// base color, inserted/deleted words are highlighted. Handles reordering and
/// scattered edits better than a plain prefix/suffix match.
fn word_diff(old: &str, new: &str) -> (Vec<Segment>, Vec<Segment>) {
    let o = split_words(old);
    let n = split_words(new);
    let diff = TextDiff::from_slices(&o, &n);
    let mut oseg: Vec<Segment> = Vec::new();
    let mut nseg: Vec<Segment> = Vec::new();
    for change in diff.iter_all_changes() {
        let text = change.value().to_string();
        match change.tag() {
            ChangeTag::Equal => {
                push_seg(&mut oseg, &text, false);
                push_seg(&mut nseg, &text, false);
            }
            ChangeTag::Delete => push_seg(&mut oseg, &text, true),
            ChangeTag::Insert => push_seg(&mut nseg, &text, true),
        }
    }
    (oseg, nseg)
}

fn push_seg(v: &mut Vec<Segment>, text: &str, changed: bool) {
    if let Some(last) = v.last_mut() {
        if last.changed == changed {
            last.text.push_str(text);
            return;
        }
    }
    v.push(Segment {
        text: text.to_string(),
        changed,
    });
}

/// Split into words while keeping whitespace as its own tokens, so alignment
/// stays natural and spacing is preserved on reassembly. Returns borrowed
/// slices into `s`.
fn split_words(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_ws = false;
    for (i, ch) in s.char_indices() {
        let ws = ch.is_whitespace();
        if i == start {
            in_ws = ws;
        } else if ws != in_ws {
            out.push(&s[start..i]);
            start = i;
            in_ws = ws;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/main.rs b/src/main.rs\n\
index 1234567..89abcde 100644\n\
--- a/src/main.rs\n\
+++ b/src/main.rs\n\
@@ -1,4 +1,4 @@\n\
 fn main() {\n\
-    println!(\"hello\");\n\
+    println!(\"hello world\");\n\
     let x = 1;\n\
 }\n";

    #[test]
    fn parses_single_file() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "src/main.rs");
        assert_eq!(files[0].hunks.len(), 1);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!(lines[1].kind, LineKind::Remove);
        assert_eq!(lines[2].kind, LineKind::Add);
        assert_eq!(lines[0].new_no, Some(1));
        assert_eq!(lines[2].new_no, Some(2));
        assert_eq!(lines[3].old_no, Some(3));
    }

    #[test]
    fn strips_mnemonic_prefixes() {
        // diff.mnemonicPrefix=true emits c/ (commit) and w/ (worktree) instead
        // of a/ b/. Both the header split and the ---/+++ lines must resolve to
        // the real path so display and open work.
        let diff = "diff --git c/src/main.rs w/src/main.rs\n\
index e69de29..0cfbf08 100644\n\
--- c/src/main.rs\n\
+++ w/src/main.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n";
        let files = parse(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "src/main.rs");
    }

    #[test]
    fn intraline_marks_changed_word() {
        let files = parse(SAMPLE);
        let lines = &files[0].hunks[0].lines;
        let add = &lines[2];
        assert!(!add.segments.is_empty(), "paired add should be segmented");
        assert!(add.segments.iter().any(|s| s.changed));
        assert!(add.segments.iter().any(|s| !s.changed));
        let joined: String = add.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, add.content);
    }

    #[test]
    fn handles_new_and_binary() {
        let d = "diff --git a/a.png b/a.png\nnew file mode 100644\nindex 0..1\nBinary files /dev/null and b/a.png differ\n";
        let files = parse(d);
        assert_eq!(files.len(), 1);
        assert!(files[0].is_binary);
        assert!(files[0].notes.iter().any(|n| n == "new file"));
    }

    #[test]
    fn multiple_files() {
        let d = format!("{SAMPLE}diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old\n+new\n");
        let files = parse(&d);
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].path(), "b.txt");
    }

    #[test]
    fn split_files_matches_parse_and_drops_preamble() {
        // A `git show` preamble before the first `diff --git` is discarded, and
        // the section count/order lines up 1:1 with parse().
        let d = format!(
            "commit abc\nAuthor: x\n\n    msg\n\n{SAMPLE}diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old\n+new\n"
        );
        let sections = split_files(&d);
        let files = parse(&d);
        assert_eq!(sections.len(), files.len());
        assert!(sections[0].starts_with("diff --git a/src/main.rs"));
        assert!(sections[1].starts_with("diff --git a/b.txt"));
        assert!(sections[1].contains("+new"));
        // No commit preamble leaked into the first section.
        assert!(!sections[0].contains("commit abc"));
    }
}
