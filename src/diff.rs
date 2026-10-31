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
    /// Combined diffs retain one line number and change marker per parent.
    pub parent_nos: Vec<Option<usize>>,
    pub parent_marks: String,
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
    pub parent_count: usize,
    pub parent_paths: Vec<String>,
    /// Keep Git's combined header rather than inventing a two-sided header.
    pub combined_header: Option<String>,
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

pub fn is_file_header(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("diff --cc ") || line.starts_with("diff --combined ")
}

/// Split a raw diff into per-file sections, one string per Git file header
/// block, in the same order (and count) as [`parse`]. Any preamble before the
/// first block (e.g. `git show` commit headers) is dropped. Used to copy an
/// exact per-file patch without reconstructing it.
pub fn split_files(input: &str) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    for line in input.lines() {
        if is_file_header(line) {
            sections.push(String::new());
        }
        if let Some(cur) = sections.last_mut() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    sections
}

/// Extract the commit metadata that precedes the first Git diff block, as
/// produced by `git show` (commit line, author/date fields, and the indented
/// message). Returns the raw lines with a single trailing blank line, so the
/// metadata is separated from the diff below it. Empty when the input has no
/// preamble (plain `git diff`, worktree/staged diffs). A commit header can
/// stand alone when Git emits no patch, as with many merge commits.
pub fn preamble(input: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut found_diff = false;
    for line in input.lines() {
        if is_file_header(line) {
            found_diff = true;
            break;
        }
        lines.push(line.to_string());
    }
    if !found_diff && !lines.first().is_some_and(|line| line.starts_with("commit ")) {
        return Vec::new();
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    // Keep one trailing blank as a separator before the diff.
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Parse unified and combined diffs from `git diff` / `git show`.
pub fn parse(input: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut lines = input.lines().peekable();

    while let Some(line) = lines.next() {
        if !is_file_header(line) {
            continue;
        }
        let combined_path = line.strip_prefix("diff --cc ").or_else(|| line.strip_prefix("diff --combined "));
        let mut file = FileDiff {
            old_path: String::new(),
            new_path: String::new(),
            hunks: Vec::new(),
            is_binary: false,
            notes: Vec::new(),
            parent_count: if combined_path.is_some() { 2 } else { 1 },
            parent_paths: Vec::new(),
            combined_header: combined_path.map(|_| line.to_string()),
        };
        if let Some(path) = combined_path {
            file.old_path = path.to_string();
            file.new_path = path.to_string();
        } else if let Some((a, b)) = parse_diff_header(line) {
            file.old_path = a;
            file.new_path = b;
        }

        // Consume the extended header until the first hunk or next file.
        let mut saw_old_path = false;
        while let Some(&next) = lines.peek() {
            if is_file_header(next) || next.starts_with("@@") {
                break;
            }
            let h = lines.next().unwrap();
            if let Some(p) = h.strip_prefix("--- ") {
                if combined_path.is_some() {
                    file.parent_paths.push(strip_prefix(p.trim()));
                }
                if !saw_old_path {
                    file.old_path = strip_prefix(p.trim());
                    saw_old_path = true;
                }
            } else if let Some(p) = h.strip_prefix("+++ ") {
                file.new_path = strip_prefix(p.trim());
            } else if file.combined_header.is_some() && h.starts_with("index ") {
                if let Some((parents, _)) = h[6..].split_once("..") {
                    file.parent_count = parents.split(',').count();
                }
            } else if h.starts_with("new file mode") {
                file.notes.push("new file".into());
            } else if h.starts_with("deleted file mode") {
                file.notes.push("deleted".into());
            } else if let Some(r) = h.strip_prefix("rename from ") {
                file.notes.push(format!("renamed from {r}"));
            } else if h.starts_with("Binary files") || h.contains("GIT binary patch") {
                file.is_binary = true;
            } else if h.starts_with("old mode") || h.starts_with("new mode")
                || (file.combined_header.is_some() && h.starts_with("mode "))
            {
                file.notes.push(h.trim().to_string());
            }
        }

        // Parse hunks.
        while let Some(&next) = lines.peek() {
            if is_file_header(next) {
                break;
            }
            let hline = lines.next().unwrap();
            if !hline.starts_with("@@") {
                continue;
            }
            let (mut old_nos, mut new_no) = parse_hunk_header(hline);
            let combined = file.combined_header.is_some();
            if combined {
                file.parent_count = old_nos.len();
            }
            let mut hunk = Hunk {
                header: hline.to_string(),
                lines: Vec::new(),
            };
            while let Some(&next) = lines.peek() {
                if next.starts_with("@@") || is_file_header(next) {
                    break;
                }
                let l = lines.next().unwrap();
                if l == "\\ No newline at end of file" {
                    continue;
                }
                let (marks, content) = if combined {
                    let Some(marks) = l.get(..old_nos.len()) else { continue };
                    if !marks.bytes().all(|b| matches!(b, b' ' | b'+' | b'-')) {
                        continue;
                    }
                    (marks, &l[marks.len()..])
                } else {
                    match l.as_bytes().first() {
                        Some(b'+' | b'-' | b' ') => (&l[..1], &l[1..]),
                        _ => (" ", l),
                    }
                };
                let removed = marks.contains('-');
                let kind = if removed { LineKind::Remove } else if marks.contains('+') { LineKind::Add } else { LineKind::Context };
                let mut old_no = None;
                let mut parent_nos = Vec::new();
                for (i, (mark, no)) in marks.bytes().zip(&mut old_nos).enumerate() {
                    let present = if removed { mark == b'-' } else { mark != b'+' };
                    let number = present.then(|| {
                        let number = *no;
                        *no += 1;
                        number
                    });
                    if i == 0 {
                        old_no = number;
                    }
                    if combined {
                        parent_nos.push(number);
                    }
                }
                let result_no = (!removed).then(|| {
                    let number = new_no;
                    new_no += 1;
                    number
                });
                hunk.lines.push(DiffLine {
                    kind,
                    content: content.to_string(),
                    old_no,
                    new_no: result_no,
                    parent_nos,
                    parent_marks: if combined { marks.to_string() } else { String::new() },
                    segments: Vec::new(),
                });
            }
            // Combined removals can come from different parents, so they have
            // no single old side to pair with additions for a word diff.
            if !combined {
                intraline(&mut hunk.lines);
            }
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

fn parse_hunk_header(line: &str) -> (Vec<usize>, usize) {
    // @@ -old_start,old_len +new_start,new_len @@ heading
    let mut old_nos = Vec::new();
    let mut new_no = 1;
    for tok in line.split_whitespace().skip(1).take_while(|tok| !tok.starts_with("@@")) {
        if let Some(rest) = tok.strip_prefix('-') {
            old_nos.push(rest.split(',').next().and_then(|s| s.parse().ok()).unwrap_or(1));
        } else if let Some(rest) = tok.strip_prefix('+') {
            new_no = rest.split(',').next().and_then(|s| s.parse().ok()).unwrap_or(1);
        }
    }
    if old_nos.is_empty() {
        old_nos.push(1);
    }
    (old_nos, new_no)
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

    #[test]
    fn preamble_captures_header_and_ends_with_one_blank() {
        let show = "commit abc123\nAuthor:     A U Thor <a@u.thor>\nAuthorDate: Mon Jan 1 00:00:00 2024\n\n    Subject line\n\n    Body paragraph.\n\n\ndiff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
        let meta = preamble(show);
        assert_eq!(meta.first().unwrap(), "commit abc123");
        assert!(meta.iter().any(|l| l == "    Subject line"));
        assert!(meta.iter().any(|l| l == "    Body paragraph."));
        // Multiple trailing blanks collapse to exactly one separator line.
        assert_eq!(meta.last().unwrap(), "");
        assert_ne!(meta[meta.len() - 2], "");
    }

    #[test]
    fn preamble_is_empty_for_plain_diff() {
        assert!(preamble(SAMPLE).is_empty());
    }

    #[test]
    fn preamble_keeps_commit_metadata_without_a_patch() {
        let show = "commit abc123\nMerge: 111 222\nAuthor: A U Thor\n\n    Clean merge\n\n";
        assert!(parse(show).is_empty());
        assert_eq!(preamble(show), ["commit abc123", "Merge: 111 222", "Author: A U Thor", "", "    Clean merge", ""]);
    }

    #[test]
    fn preamble_recognizes_combined_diffs() {
        let cc = "commit abc123\nMerge: 111 222\n\n    Merge branch\n\ndiff --cc f\nindex 1,2..3\n--- a/f\n+++ b/f\n@@@ -1,1 -1,1 +1,1 @@@\n- a\n +b\n";
        assert_eq!(preamble(cc), ["commit abc123", "Merge: 111 222", "", "    Merge branch", ""]);
        // Arbitrary non-diff text is likewise not a preamble.
        assert!(preamble("just some text\nwith no diff\n").is_empty());
    }

    const COMBINED: &str = include_str!("../demo/merge.diff");

    #[test]
    fn parses_combined_parent_columns_and_result_numbers() {
        let files = parse(COMBINED);
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(file.path(), "merge.txt");
        assert_eq!(file.parent_count, 2);
        assert_eq!(file.combined_header.as_deref(), Some("diff --cc merge.txt"));
        assert_eq!(file.stats(), (3, 2));
        let lines = &file.hunks[0].lines;
        let expected = [
            ("  ", "start", vec![Some(1), Some(1)], Some(1), LineKind::Context),
            ("- ", "left version", vec![Some(2), None], None, LineKind::Remove),
            (" -", "right version", vec![None, Some(2)], None, LineKind::Remove),
            ("++", "merged version", vec![None, None], Some(2), LineKind::Add),
            (" +", "left kept", vec![Some(3), None], Some(3), LineKind::Add),
            ("+ ", "right kept", vec![None, Some(3)], Some(4), LineKind::Add),
            ("  ", "end", vec![Some(4), Some(4)], Some(5), LineKind::Context),
        ];
        assert_eq!(lines.len(), expected.len());
        for (line, (marks, content, parents, result, kind)) in lines.iter().zip(expected) {
            assert_eq!(line.parent_marks, marks);
            assert_eq!(line.content, content);
            assert_eq!(line.old_no, parents[0]);
            assert_eq!(line.parent_nos, parents);
            assert_eq!(line.new_no, result);
            assert_eq!(line.kind, kind);
            assert!(line.segments.is_empty());
        }
    }

    #[test]
    fn parses_three_parents_and_all_paths() {
        let patch = "diff --combined renamed.txt\nindex 1,2,3..4\n--- a/first.txt\n--- a/second.txt\n--- a/third.txt\n+++ b/renamed.txt\n@@@@ -10,3 -20,3 -30,3 +40,4 @@@@ heading -not-a-range\n   shared\n-  left\n --others\n+++merged\n ++first kept\n+ +second kept\n  -third removed\n\\ No newline at end of file\n";
        let files = parse(patch);
        let file = &files[0];
        assert_eq!(file.parent_count, 3);
        assert_eq!(file.old_path, "first.txt");
        assert_eq!(file.parent_paths, ["first.txt", "second.txt", "third.txt"]);
        assert_eq!(file.new_path, "renamed.txt");
        assert_eq!(file.stats(), (3, 3));
        let lines = &file.hunks[0].lines;
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0].parent_nos, [Some(10), Some(20), Some(30)]);
        assert_eq!(lines[2].parent_nos, [None, Some(21), Some(31)]);
        assert_eq!(lines[4].parent_nos, [Some(12), None, None]);
        assert_eq!(lines[5].parent_nos, [None, Some(22), None]);
        assert_eq!(lines[5].new_no, Some(43));
        assert_eq!(lines[6].parent_nos, [None, None, Some(32)]);
        assert_eq!(lines[6].new_no, None);
    }

    #[test]
    fn combined_hunks_reset_numbers_and_preserve_unicode() {
        let patch = "diff --cc 文書.txt\n--- a/文書.txt\n+++ b/文書.txt\n@@@ -1 -2 +0,0 @@@\n--旧内容\n@@@ -10,0 -20,0 +30 @@@\n++新内容\n";
        let files = parse(patch);
        let file = &files[0];
        assert_eq!(file.path(), "文書.txt");
        assert_eq!(file.hunks.len(), 2);
        assert_eq!(file.hunks[0].lines[0].parent_nos, [Some(1), Some(2)]);
        assert_eq!(file.hunks[0].lines[0].new_no, None);
        assert_eq!(file.hunks[0].lines[0].content, "旧内容");
        assert_eq!(file.hunks[1].lines[0].parent_nos, [None, None]);
        assert_eq!(file.hunks[1].lines[0].new_no, Some(30));
        assert_eq!(file.hunks[1].lines[0].content, "新内容");
    }

    #[test]
    fn combined_binary_and_mixed_blocks_stay_separate() {
        let binary = "diff --combined image.png\nindex 1,2,3..4\nBinary files differ\n";
        let patch = format!("commit abc\nMerge: 1 2\n\n{COMBINED}{SAMPLE}{binary}");
        let files = parse(&patch);
        let blocks = split_files(&patch);
        assert_eq!(files.len(), 3);
        assert_eq!(blocks, [COMBINED.replace("\r\n", "\n"), SAMPLE.to_string(), binary.to_string()]);
        let crlf = patch.replace("\r\n", "\n").replace('\n', "\r\n");
        assert_eq!(split_files(&crlf), blocks);
        assert_eq!(files[1].parent_count, 1);
        assert!(files[1].combined_header.is_none());
        assert_eq!(files[2].parent_count, 3);
        assert!(files[2].is_binary);
        for (file, block) in files.iter().zip(&blocks) {
            let streamed = parse(block);
            assert_eq!(streamed.len(), 1);
            assert_eq!(file.path(), streamed[0].path());
            assert_eq!(file.stats(), streamed[0].stats());
        }
        assert_eq!(preamble(&patch), ["commit abc", "Merge: 1 2", ""]);
    }

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
    fn strips_ansi_so_colorized_diff_parses() {
        // Git colorizes the diff it pipes to a pager (color.diff auto-on for
        // pagers). The raw bytes carry SGR codes and mnemonic i/ w/ prefixes,
        // exactly as captured from `git -c pager.diff=drift diff`.
        let colored = "\x1b[33mdiff --git i/f.txt w/f.txt\x1b[m\n\
\x1b[33mindex de98044..4657bb7 100644\x1b[m\n\
\x1b[33m--- i/f.txt\x1b[m\n\
\x1b[33m+++ w/f.txt\x1b[m\n\
\x1b[36m@@ -1,3 +1,3 @@\x1b[m\n\
 a\n\
\x1b[31m-b\x1b[m\n\
\x1b[32m+CHANGED\x1b[m\n\
 c\n";
        // Raw colorized text parses to nothing (the bug).
        assert_eq!(parse(colored).len(), 0);
        // After stripping it parses cleanly.
        let files = parse(&uncurses::ansi::strip::strip(colored));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "f.txt");
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines[1].kind, LineKind::Remove);
        assert_eq!(lines[2].kind, LineKind::Add);
        assert_eq!(lines[2].content, "CHANGED");
        assert!(!lines[2].content.contains('\x1b'));
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
    fn intraline_handles_cjk_and_accents() {
        // Multibyte content must parse intact and word-diff on char boundaries
        // (never byte offsets): CJK words and accented Latin, with a space so
        // the shared prefix stays unchanged and only the last word is marked.
        let d = "diff --git a/文書.txt b/文書.txt\n\
index 1..2 100644\n\
--- a/文書.txt\n\
+++ b/文書.txt\n\
@@ -1,2 +1,2 @@\n\
-你好 世界\n\
+你好 宇宙\n\
-café crème\n\
+café glacé\n";
        let files = parse(d);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path(), "文書.txt");
        let lines = &files[0].hunks[0].lines;
        let cjk_add = &lines[1];
        assert_eq!(cjk_add.content, "你好 宇宙");
        // Segments must reassemble to the exact content — proof no multibyte
        // char was split or dropped by the word-diff.
        let joined: String = cjk_add.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, cjk_add.content);
        assert!(cjk_add.segments.iter().any(|s| s.changed && s.text.contains("宇宙")));
        assert!(cjk_add.segments.iter().any(|s| !s.changed && s.text.contains("你好")));
        // Accented Latin behaves the same.
        let acc_add = &lines[3];
        let joined: String = acc_add.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, acc_add.content);
        assert!(acc_add.segments.iter().any(|s| s.changed && s.text.contains("glacé")));
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

    #[test]
    fn per_block_parse_equals_whole() {
        // The stdin streamer (tui::spawn_stream) parses each `diff --git` block
        // as it arrives instead of the whole diff at once. That's only correct
        // if parsing blocks independently yields the same files, in order, as
        // parsing everything together. Lock that invariant.
        let d = format!(
            "commit abc\n\n{SAMPLE}diff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-old\n+new\n"
        );
        let whole = parse(&d);
        let streamed: Vec<FileDiff> = split_files(&d)
            .iter()
            .filter_map(|block| parse(block).into_iter().next())
            .collect();
        assert_eq!(streamed.len(), whole.len());
        for (s, w) in streamed.iter().zip(&whole) {
            assert_eq!(s.path(), w.path());
            assert_eq!(s.hunks.len(), w.hunks.len());
            assert_eq!(s.is_binary, w.is_binary);
        }
    }
}
