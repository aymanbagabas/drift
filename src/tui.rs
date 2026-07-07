//! The terminal UI: a scrollable, syntax-highlighted diff pane with a single
//! bottom footer, driven by an uncurses `Screen`.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use uncurses::buffer::{Bounded, SurfaceMut};
use uncurses::color::Color;
use uncurses::event::{Event, MouseButton};
use uncurses::screen::{MouseTracking, Screen, ScreenOptions};
use uncurses::style::Style;
use uncurses::terminal::{TtyInput, TtyOutput};
use uncurses::text::TextSurface;

use crate::config::{parse_style, Config, Palette};
use crate::diff::{self, FileDiff, LineKind};
use crate::git::Source;
use crate::highlight::Highlighter;

/// A resolved styling palette built once from config. Diff-body styles plus
/// component-named chrome styles (`statusbar_*`, `help_*`, `dialog_*`), all
/// derived from the configurable color palette and style specs.
struct Theme {
    add: Style,
    remove: Style,
    context: Style,
    header: Style,
    line_number: Style,
    add_emph_bg: Option<Color>,
    remove_emph_bg: Option<Color>,
    add_line_bg: Option<Color>,
    remove_line_bg: Option<Color>,
    cursor_bg: Color,
    // Status bar components.
    statusbar: Style,
    statusbar_logo: Style,
    statusbar_filename: Style,
    statusbar_add: Style,
    statusbar_remove: Style,
    statusbar_flags: Style,
    statusbar_stats: Style,
    statusbar_watch: Style,
    statusbar_help: Style,
    // Help grid.
    help_key: Style,
    help_desc: Style,
    // Dialogs.
    dialog: Style,
    dialog_border: Style,
}

impl Theme {
    fn from_config(c: &Config) -> Self {
        let pal = Palette::new(&c.colors);
        let sty = |name: &str, default: &str| {
            let spec = c.styles.get(name).map(|s| s.as_str()).unwrap_or(default);
            parse_style(spec, &pal)
        };
        Theme {
            add: sty("add", "add"),
            remove: sty("remove", "remove"),
            context: sty("context", "context"),
            header: sty("header", "header bold"),
            line_number: sty("line_number", "line_number"),
            add_emph_bg: pal.color("add_emph"),
            remove_emph_bg: pal.color("remove_emph"),
            add_line_bg: pal.color("add_line"),
            remove_line_bg: pal.color("remove_line"),
            cursor_bg: pal.color("cursor").unwrap_or(Color::Indexed(237)),
            statusbar: sty("statusbar", "foreground surface"),
            statusbar_logo: sty("statusbar_logo", "background primary bold"),
            statusbar_filename: sty("statusbar_filename", "foreground surface bold"),
            statusbar_add: sty("statusbar_add", "add surface"),
            statusbar_remove: sty("statusbar_remove", "remove surface"),
            statusbar_flags: sty("statusbar_flags", "muted surface"),
            statusbar_stats: sty("statusbar_stats", "foreground surface"),
            statusbar_watch: sty("statusbar_watch", "background add bold"),
            statusbar_help: sty("statusbar_help", "background secondary bold"),
            help_key: sty("help_key", "muted bold"),
            help_desc: sty("help_desc", "muted faint"),
            dialog: sty("dialog", "foreground background"),
            dialog_border: sty("dialog_border", "secondary background"),
        }
    }
}

/// One styled run of text within a display row.
struct Span {
    fg: Option<Color>,
    changed: bool,
    text: String,
}

#[derive(Clone, Copy, PartialEq)]
enum RowKind {
    Hunk,
    Add,
    Remove,
    Context,
    Note,
}

/// Which top-level view is showing.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Diff,
    Stat,
}

/// Which line-number gutter a row draws: both columns (unified), or just the
/// old/new side (each pane of the split view).
#[derive(Clone, Copy)]
enum Gut {
    Both,
    Old,
    New,
}

struct Row {
    kind: RowKind,
    old_no: Option<usize>,
    new_no: Option<usize>,
    spans: Vec<Span>,
}

pub struct App {
    screen: Screen<TtyInput, TtyOutput>,
    config: Config,
    theme: Theme,
    highlighter: Arc<Highlighter>,
    source: Source,
    opts: crate::git::Opts,
    toplevel: Option<PathBuf>,
    files: Vec<FileDiff>,
    /// Precomputed display rows per file, built lazily and kept until reload.
    row_cache: Vec<Option<Vec<Row>>>,
    /// Background row builder feeding `row_cache` after startup so the first
    /// frame isn't blocked on syntax highlighting.
    prefetch: Option<Receiver<(usize, Vec<Row>)>>,
    selected: usize,
    /// Top visible row (viewport offset).
    scroll: usize,
    /// Selected/highlighted row within the diff (tig-style cursor line).
    cursor: usize,
    view: View,
    /// Side-by-side (split) diff rendering, toggled with `s`.
    split: bool,
    help_open: bool,
    /// Last window title pushed to the terminal, to avoid redundant writes.
    title: String,
    /// Whether watch mode reacts to git changes (toggle with `w`).
    watch: bool,
    /// Extra lines of context added on top of the base setting, grown by
    /// expanding folded regions with Enter on a hunk header.
    expand: usize,
}

impl App {
    pub fn new(config: Config, source: Source, opts: crate::git::Opts) -> io::Result<Self> {
        let highlighter = Arc::new(Highlighter::new(&config.theme, config.syntax));
        let theme = Theme::from_config(&config);
        // Read input/output from the controlling terminal (/dev/tty) so the
        // TUI works even when a diff is piped in on stdin (pager mode).
        let mut screen = Screen::open()?;
        screen.init_with(ScreenOptions {
            mouse: Some(MouseTracking::empty()),
            ..ScreenOptions::default()
        })?;
        screen.enter_alt_screen()?;
        screen.hide_cursor()?;
        // Enable mouse now rather than waiting for the capability-query reply,
        // so clicks and wheel work immediately (and in non-interactive tests).
        screen.enable_mouse(MouseTracking::empty())?;
        let mut app = App {
            screen,
            config,
            theme,
            highlighter,
            source,
            opts,
            toplevel: crate::git::toplevel(),
            files: Vec::new(),
            row_cache: Vec::new(),
            prefetch: None,
            selected: 0,
            scroll: 0,
            cursor: 0,
            view: View::Diff,
            split: false,
            help_open: false,
            title: String::new(),
            watch: false,
            expand: 0,
        };
        app.start();
        Ok(app)
    }

    /// Initial load used at startup: run the diff, parse it, then hand row
    /// building off to a background thread so the first frame paints without
    /// blocking on syntax highlighting.
    fn start(&mut self) {
        match self.source.diff(&self.effective_opts()) {
            Ok(text) => self.files = diff::parse(&text),
            Err(_) => self.files.clear(),
        }
        self.selected = 0;
        self.cursor = 0;
        self.scroll = 0;
        self.row_cache = (0..self.files.len()).map(|_| None).collect();
        self.spawn_prefetch();
    }

    /// Spawn a worker that builds every file's rows (the selected file first so
    /// it shows soonest) and streams them back over a channel.
    fn spawn_prefetch(&mut self) {
        let files = Arc::new(self.files.clone());
        let hl = Arc::clone(&self.highlighter);
        let intraline = self.config.intraline;
        let sel = self.selected;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let n = files.len();
            let order = std::iter::once(sel).chain((0..n).filter(|&i| i != sel));
            for idx in order {
                let rows = build_file_rows(&files[idx], &hl, intraline);
                if tx.send((idx, rows)).is_err() {
                    return; // receiver dropped (reload/quit): stop early.
                }
            }
        });
        self.prefetch = Some(rx);
    }

    /// Move any rows the prefetch worker has finished into the cache. Returns
    /// whether the selected file's rows just landed (so the caller can repaint).
    fn drain_prefetch(&mut self) {
        let Some(rx) = self.prefetch.take() else {
            return;
        };
        let mut selected_ready = false;
        loop {
            match rx.try_recv() {
                Ok((idx, rows)) => {
                    if let Some(slot) = self.row_cache.get_mut(idx) {
                        if slot.is_none() {
                            *slot = Some(rows);
                            if idx == self.selected {
                                selected_ready = true;
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.prefetch = Some(rx);
                    break;
                }
                Err(TryRecvError::Disconnected) => break, // worker done: drop rx.
            }
        }
        if selected_ready {
            self.move_cursor(0);
        }
    }

    /// The base diff options plus any extra context from expanded folds.
    fn effective_opts(&self) -> crate::git::Opts {
        let mut o = self.opts.clone();
        if self.expand > 0 {
            o.context = Some(self.opts.context.unwrap_or(3) + self.expand);
        }
        o
    }

    /// Re-run the diff source and rebuild the view, keeping the cursor near
    /// where it was so refreshes and fold-expansions aren't jarring.
    fn reload(&mut self) {
        let (sel, cur) = (self.selected, self.cursor);
        // Drop any in-flight startup prefetch so its (now stale) rows can't
        // land in the freshly rebuilt cache.
        self.prefetch = None;
        match self.source.diff(&self.effective_opts()) {
            Ok(text) => self.files = diff::parse(&text),
            Err(_) => self.files.clear(),
        }
        self.selected = sel.min(self.files.len().saturating_sub(1));
        // Invalidate the per-file row cache; rows are rebuilt lazily on view.
        self.row_cache = (0..self.files.len()).map(|_| None).collect();
        self.ensure_rows();
        self.cursor = cur.min(self.rows().len().saturating_sub(1));
        self.scroll = 0;
        self.move_cursor(0);
    }

    /// Grow the folded context around the current hunk, then pin that hunk to
    /// the same screen row so expanding doesn't scroll the view.
    /// ponytail: anchors by hunk ordinal; if a big expansion merges adjacent
    /// hunks the ordinal can shift and we fall back to reload's position.
    fn expand_here(&mut self) {
        let screen_off = self.cursor.saturating_sub(self.scroll);
        let ord = self.hunk_ordinal(self.cursor);
        self.expand += 10;
        self.reload();
        if let Some(idx) = self.nth_hunk_row(ord) {
            self.cursor = idx;
            self.scroll = idx.saturating_sub(screen_off);
            self.move_cursor(0);
        }
    }

    /// 0-based index of the hunk the cursor sits on (or the most recent above).
    fn hunk_ordinal(&self, cursor: usize) -> usize {
        self.rows()
            .iter()
            .take(cursor + 1)
            .filter(|r| r.kind == RowKind::Hunk)
            .count()
            .saturating_sub(1)
    }

    /// Row index of the nth (0-based) hunk header.
    fn nth_hunk_row(&self, ord: usize) -> Option<usize> {
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::Hunk)
            .nth(ord)
            .map(|(i, _)| i)
    }

    /// The display rows for the selected file (empty if none).
    fn rows(&self) -> &[Row] {
        self.row_cache
            .get(self.selected)
            .and_then(|o| o.as_deref())
            .unwrap_or(&[])
    }

    /// Whether the cursor is currently on a hunk header row.
    fn on_hunk(&self) -> bool {
        self.rows()
            .get(self.cursor)
            .is_some_and(|r| r.kind == RowKind::Hunk)
    }

    /// Build the selected file's rows if they aren't cached yet.
    fn ensure_rows(&mut self) {
        if self
            .row_cache
            .get(self.selected)
            .is_some_and(|o| o.is_none())
        {
            let rows = self.build_rows(self.selected);
            self.row_cache[self.selected] = Some(rows);
        }
    }

    /// Flatten a file into styled display rows (syntax + intra-line spans).
    fn build_rows(&self, idx: usize) -> Vec<Row> {
        match self.files.get(idx) {
            Some(file) => build_file_rows(file, &self.highlighter, self.config.intraline),
            None => Vec::new(),
        }
    }

    fn viewport_rows(&self) -> usize {
        (self.screen.height() as usize).saturating_sub(self.chrome_h())
    }

    /// Rows reserved at the bottom: the footer bar, plus the expanded help
    /// grid when it's open.
    fn chrome_h(&self) -> usize {
        1 + if self.help_open {
            self.help_grid().1
        } else {
            0
        }
    }

    /// The quick-help entries shown in the expandable footer grid.
    fn help_entries() -> &'static [(&'static str, &'static str)] {
        &[
            ("j/k ↑/↓", "move"),
            ("^d/^u", "half page"),
            ("g/G", "top/bottom"),
            ("{ }", "prev/next hunk"),
            ("n/p ←/→", "prev/next file"),
            ("tab", "cycle files"),
            ("s", "split view"),
            ("f", "files"),
            ("w", "watch on/off"),
            ("enter", "expand / open"),
            ("e", "edit in $EDITOR"),
            ("r", "refresh"),
            ("?", "toggle help"),
            ("q", "quit"),
        ]
    }

    /// Grid geometry for the help footer: (columns, rows, cell width). Packs
    /// entries into as many columns as fit the terminal width, charm-style.
    fn help_grid(&self) -> (usize, usize, usize) {
        let entries = Self::help_entries();
        let cell_w = entries
            .iter()
            .map(|(k, v)| k.chars().count() + 2 + v.chars().count())
            .max()
            .unwrap_or(12)
            + 3; // gap between columns
        let w = self.screen.width() as usize;
        let cols = ((w.saturating_sub(1)) / cell_w).clamp(1, entries.len());
        let rows = entries.len().div_ceil(cols);
        (cols, rows, cell_w)
    }

    fn max_scroll(&self) -> usize {
        self.rows().len().saturating_sub(self.viewport_rows())
    }

    /// Move the cursor line by `delta`, then scroll the viewport just enough to
    /// keep the cursor visible (tig-style).
    fn move_cursor(&mut self, delta: isize) {
        if self.rows().is_empty() {
            return;
        }
        let last = self.rows().len() - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last as isize) as usize;
        let vh = self.viewport_rows();
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + vh {
            self.scroll = self.cursor + 1 - vh;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn cursor_to(&mut self, idx: usize) {
        self.cursor = 0;
        self.scroll = 0;
        self.move_cursor(idx as isize);
    }

    /// Move the cursor to an absolute row without resetting the viewport.
    fn set_cursor(&mut self, idx: usize) {
        let last = self.rows().len().saturating_sub(1);
        self.cursor = idx.min(last);
        self.move_cursor(0);
    }

    fn select_file(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        let n = self.files.len() as isize;
        let new = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        self.select_file_at(new);
    }

    /// Select a file by absolute index.
    fn select_file_at(&mut self, idx: usize) {
        if idx < self.files.len() && idx != self.selected {
            self.selected = idx;
            self.scroll = 0;
            self.cursor = 0;
            self.ensure_rows();
        }
    }

    /// Open the file (and the cursor line) in the editor.
    fn open_editor(&mut self) -> io::Result<()> {
        let Some(file) = self.files.get(self.selected) else {
            return Ok(());
        };
        // Prefer the cursor row's line; fall back to the next content row.
        let line = self
            .rows()
            .iter()
            .skip(self.cursor)
            .find_map(|r| r.new_no.or(r.old_no))
            .or_else(|| self.rows().iter().find_map(|r| r.new_no.or(r.old_no)))
            .unwrap_or(1);
        let rel = file.path();
        let path = match &self.toplevel {
            Some(top) => top.join(rel),
            None => PathBuf::from(rel),
        };
        let editor = self.config.editor_cmd();
        let mut parts = editor.split_whitespace();
        let Some(bin) = parts.next() else {
            return Ok(());
        };
        let args: Vec<String> = parts.map(String::from).collect();

        self.screen.pause()?;
        // ponytail: `+LINE file` covers vi/nano/emacs/less; a picker for
        // editor-specific syntax (code --goto) can come if anyone asks.
        let _ = std::process::Command::new(bin)
            .args(&args)
            .arg(format!("+{line}"))
            .arg(&path)
            .status();
        self.screen.resume()?;
        Ok(())
    }

    pub fn run(&mut self, refresh: Option<Receiver<()>>, watch: bool) -> io::Result<()> {
        self.watch = watch;
        loop {
            // Fold in any rows the startup worker has finished.
            self.drain_prefetch();
            self.render()?;
            // Poll faster while the prefetch is still streaming so freshly
            // parsed files appear promptly; idle at 200ms once it's done.
            let timeout = if self.prefetch.is_some() {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(200)
            };
            if self.screen.poll_event(Some(timeout))? {
                // Drain every queued event before the next render so bursts
                // (held keys, fast scrolling, paste) stay responsive.
                while let Some(ev) = self.screen.try_read_event() {
                    if self.handle(ev)? {
                        return Ok(());
                    }
                }
            } else if let Some(rx) = &refresh {
                // Always drain queued notifications so they don't pile up, but
                // only reload when watch mode is on.
                if rx.try_recv().is_ok() {
                    while rx.try_recv().is_ok() {}
                    if self.watch {
                        self.reload();
                    }
                }
            }
        }
    }

    /// Handle one event. Returns Ok(true) when the app should quit.
    fn handle(&mut self, ev: Event) -> io::Result<bool> {
        let page = self.viewport_rows() as isize;
        match ev {
            Event::KeyPress(k) => {
                // Match with the uncurses key matcher: `matches` compares the
                // produced glyph (so shifted symbols like `}` and uppercase
                // synonyms work) and falls back to named-key patterns.
                // The help grid is an inline footer, not a blocking overlay:
                // `?`/esc toggle it off, every other key still works normally.
                if self.help_open {
                    if k.matches_any(["q", "ctrl+c"]) {
                        return Ok(true);
                    }
                    if k.matches_any(["?", "escape"]) {
                        self.help_open = false;
                        return Ok(false);
                    }
                }
                if self.view == View::Stat {
                    if k.matches_any(["q", "ctrl+c"]) {
                        return Ok(true);
                    } else if k.matches_any(["j", "down", "right"]) {
                        self.select_file(1);
                    } else if k.matches_any(["k", "up", "left"]) {
                        self.select_file(-1);
                    } else if k.matches_any(["g", "home"]) {
                        self.selected = 0;
                    } else if k.matches_any(["G", "end"]) {
                        self.selected = self.files.len().saturating_sub(1);
                    } else if k.matches_any(["f", "tab", "escape"]) {
                        self.view = View::Diff;
                    } else if k.matches("?") {
                        self.help_open = !self.help_open;
                    } else if k.matches_any(["enter", "e"]) {
                        self.view = View::Diff;
                        self.ensure_rows();
                        self.cursor_to(0);
                    } else if k.matches("r") {
                        self.reload();
                    }
                    return Ok(false);
                }
                if k.matches_any(["q", "escape", "ctrl+c"]) {
                    return Ok(true);
                } else if k.matches_any(["j", "down"]) {
                    self.move_cursor(1);
                } else if k.matches_any(["k", "up"]) {
                    self.move_cursor(-1);
                } else if k.matches_any(["ctrl+d", "pagedown", "space"]) {
                    self.move_cursor(page / 2);
                } else if k.matches_any(["ctrl+u", "pageup"]) {
                    self.move_cursor(-page / 2);
                } else if k.matches_any(["g", "home"]) {
                    self.cursor_to(0);
                } else if k.matches_any(["G", "end"]) {
                    self.cursor_to(self.rows().len().saturating_sub(1));
                } else if k.matches_any(["}", ")"]) {
                    self.jump_hunk(1);
                } else if k.matches_any(["{", "("]) {
                    self.jump_hunk(-1);
                } else if k.matches_any(["n", "tab", "]", "right"]) {
                    self.select_file(1);
                } else if k.matches_any(["p", "shift+tab", "[", "left"]) {
                    self.select_file(-1);
                } else if k.matches("s") {
                    self.split = !self.split;
                } else if k.matches("f") {
                    self.view = View::Stat;
                } else if k.matches("w") {
                    self.watch = !self.watch;
                } else if k.matches("?") {
                    self.help_open = !self.help_open;
                } else if k.matches("e") {
                    self.open_editor()?;
                } else if k.matches("enter") {
                    // Enter expands folded context on a hunk header, otherwise
                    // opens the file at the cursor line.
                    if self.on_hunk() && !matches!(self.source, Source::Stdin) {
                        self.expand_here();
                    } else {
                        self.open_editor()?;
                    }
                } else if k.matches("r") {
                    self.reload();
                }
            }
            Event::MouseWheel(m) => {
                if m.button == MouseButton::WheelUp {
                    self.move_cursor(-3);
                } else if m.button == MouseButton::WheelDown {
                    self.move_cursor(3);
                }
            }
            Event::MouseClick(m) => {
                if m.button == MouseButton::Left && self.view == View::Diff && !self.help_open {
                    let h = self.screen.height();
                    // Body rows sit above the footer (last row).
                    if m.y + 1 < h {
                        let row = m.y as usize;
                        self.set_cursor(self.scroll + row);
                    }
                }
            }
            Event::Resize(ws) => {
                self.screen.resize((ws.col, ws.row));
                self.move_cursor(0);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Move the cursor to the next/previous hunk header row.
    fn jump_hunk(&mut self, dir: isize) {
        let cur = self.cursor;
        let target = if dir > 0 {
            self.rows()
                .iter()
                .enumerate()
                .skip(cur + 1)
                .find(|(_, r)| r.kind == RowKind::Hunk)
                .map(|(i, _)| i)
        } else {
            self.rows()
                .iter()
                .enumerate()
                .take(cur)
                .rev()
                .find(|(_, r)| r.kind == RowKind::Hunk)
                .map(|(i, _)| i)
        };
        if let Some(i) = target {
            self.move_cursor(i as isize - cur as isize);
        }
    }

    /// Set the terminal window title to "<filename> ● difft", updating only
    /// when it changes so we don't emit an OSC on every frame.
    fn update_title(&mut self) -> io::Result<()> {
        let want = match self.files.get(self.selected) {
            Some(f) => format!("{} · difft", f.path()),
            None => "difft".to_string(),
        };
        if want != self.title {
            self.screen.set_title(&want)?;
            self.title = want;
        }
        Ok(())
    }

    fn render(&mut self) -> io::Result<()> {
        self.update_title()?;
        self.screen.clear();
        let w = self.screen.width();
        let h = self.screen.height();
        if w < 20 || h < 4 {
            self.screen.set_str((0, 0), "terminal too small", Style::default());
            return self.screen.render();
        }

        let chrome = self.chrome_h() as u16;
        let body_h = h.saturating_sub(chrome);

        // The diff fills the whole body: full width, starting at the top.
        if self.split {
            self.render_split(0, w, body_h);
        } else {
            self.render_diff(0, w, body_h);
        }

        if self.view == View::Stat {
            self.render_stat_modal();
        }

        // Footer bar sits just above the help grid (when the grid is open the
        // footer is "pushed up" to make room below it).
        let footer_row = body_h;
        self.render_footer(footer_row);
        if self.help_open {
            self.render_help_grid(footer_row + 1, h);
        }
        self.screen.render()
    }

    /// The single bottom footer: a bold "difft" badge, the current file name,
    /// its stats and flags on a subtle chip, then right-aligned global stats, a
    /// watch indicator, and a "? help" badge.
    fn render_footer(&mut self, row: u16) {
        let w = self.screen.width();
        let (nf, add, del) = diff::totals(&self.files);
        let file = self.files.get(self.selected);
        let name = file.map(|f| f.path()).unwrap_or("(no changes)").to_string();
        let notes = file
            .filter(|f| !f.notes.is_empty())
            .map(|f| format!(" ({})", f.notes.join(", ")))
            .unwrap_or_default();

        let bar = self.theme.statusbar.clone();

        // Base fill for the whole bar.
        self.screen
            .set_str((0, row), &" ".repeat(w as usize), bar.clone());

        // Right edge, laid out right-to-left: "? help" badge, the watch badge
        // (only when watch mode is on), then the global diffstat.
        let help_badge = " ? help ";
        let help_x = w.saturating_sub(help_badge.chars().count() as u16);
        self.screen
            .set_str((help_x, row), help_badge, self.theme.statusbar_help.clone());

        let w_x = if self.watch {
            let w_badge = " W ";
            let wx = help_x.saturating_sub(w_badge.chars().count() as u16);
            self.screen
                .set_str((wx, row), w_badge, self.theme.statusbar_watch.clone());
            wx
        } else {
            help_x
        };

        // Global stats.
        let stats = format!(" {nf} files +{add} -{del} ");
        let stats_x = w_x.saturating_sub(stats.chars().count() as u16);
        self.screen
            .set_str((stats_x, row), &stats, self.theme.statusbar_stats.clone());

        // Left: bold "difft" badge in the primary accent.
        let app = " difft ";
        self.screen
            .set_str((0, row), app, self.theme.statusbar_logo.clone());
        let mut x = app.chars().count() as u16;

        // File name.
        let name_seg = format!(" {name}");
        let name_clip = clip(&name_seg, stats_x.saturating_sub(x));
        self.screen
            .set_str((x, row), &name_clip, self.theme.statusbar_filename.clone());
        x += name_clip.chars().count() as u16;

        // Per-file line stats and flags, a muted group next to the file name.
        if let Some(f) = file {
            let (fa, fd) = f.stats();
            let put = |s: &mut Self, x: &mut u16, text: &str, style: Style| {
                if *x >= stats_x {
                    return;
                }
                let t = clip(text, stats_x - *x);
                s.screen.set_str((*x, row), &t, style);
                *x += t.chars().count() as u16;
            };
            put(self, &mut x, " +", self.theme.statusbar_add.clone());
            put(self, &mut x, &fa.to_string(), self.theme.statusbar_add.clone());
            put(self, &mut x, " -", self.theme.statusbar_remove.clone());
            put(self, &mut x, &fd.to_string(), self.theme.statusbar_remove.clone());
            if !notes.is_empty() {
                put(self, &mut x, &notes, self.theme.statusbar_flags.clone());
            }
            put(self, &mut x, " ", bar.clone());
        }
    }

    /// The expandable help grid drawn below the footer, packing key/description
    /// pairs into as many columns as fit (charm-style).
    fn render_help_grid(&mut self, y0: u16, h: u16) {
        let (_, rows, cell_w) = self.help_grid();
        let entries = Self::help_entries();
        let key_style = self.theme.help_key.clone();
        let desc_style = self.theme.help_desc.clone();
        // Descriptions align to a fixed column so keys and descriptions line up
        // regardless of individual key width.
        let key_w = entries.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0) as u16;
        for (i, (k, v)) in entries.iter().enumerate() {
            let col = i / rows;
            let r = i % rows;
            let y = y0 + r as u16;
            if y >= h {
                continue;
            }
            let x = 1 + col as u16 * cell_w as u16;
            self.screen.set_str((x, y), k, key_style.clone());
            let dx = x + key_w + 2;
            self.screen.set_str((dx, y), v, desc_style.clone());
        }
    }

    /// Draw a centered dialog with rounded borders, returning the inner
    /// top-left corner. Fills the interior so content underneath is hidden.
    fn draw_box(&mut self, inner_w: u16, inner_h: u16) -> (u16, u16) {
        let w = self.screen.width();
        let h = self.screen.height();
        let bw = (inner_w + 2).min(w);
        let bh = (inner_h + 2).min(h);
        let x0 = (w - bw) / 2;
        let y0 = (h - bh) / 2;
        // Rounded borders and interior share the dialog surface so the dialog
        // reads as one clean panel.
        let border = self.theme.dialog_border.clone();
        let fill = self.theme.dialog.clone();

        let top = format!("╭{}╮", "─".repeat((bw - 2) as usize));
        let bottom = format!("╰{}╯", "─".repeat((bw - 2) as usize));
        self.screen.set_str((x0, y0), &top, border.clone());
        self.screen.set_str((x0, y0 + bh - 1), &bottom, border.clone());
        for row in 1..bh - 1 {
            self.screen.set_str((x0, y0 + row), "│", border.clone());
            self.screen
                .set_str((x0 + 1, y0 + row), &" ".repeat((bw - 2) as usize), fill.clone());
            self.screen.set_str((x0 + bw - 1, y0 + row), "│", border.clone());
        }
        (x0 + 1, y0 + 1)
    }

    /// Modal diffstat: file names with a scaled, colored +/- bar, like
    /// `git diff --stat`, floating over the diff on a secondary-accent surface.
    fn render_stat_modal(&mut self) {
        let w = self.screen.width();
        let h = self.screen.height();
        let (nf, add, del) = diff::totals(&self.files);
        let on_bg = self.theme.dialog.clone();

        if self.files.is_empty() {
            let (ix, iy) = self.draw_box(24, 1);
            self.screen.set_str((ix, iy), "no changes", on_bg.clone());
            return;
        }

        let name_w = self
            .files
            .iter()
            .map(|f| f.path().chars().count())
            .max()
            .unwrap_or(10)
            .clamp(10, (w as usize).saturating_sub(24));
        let count_w = 5usize;
        let bar_w = 24usize.min((w as usize).saturating_sub(name_w + count_w + 8));
        let inner_w = (name_w + count_w + bar_w + 4) as u16;
        // Rows: one per file (capped to fit) + a blank + summary line.
        let max_rows = (h.saturating_sub(6)) as usize;
        let list_h = self.files.len().min(max_rows.max(1));
        let inner_h = (list_h + 2) as u16;
        let (ix, iy) = self.draw_box(inner_w, inner_h);

        // Scroll the list so the selected file stays visible.
        let start = self.selected.saturating_sub(list_h.saturating_sub(1));
        let max_count = self
            .files
            .iter()
            .map(|f| {
                let (a, d) = f.stats();
                a + d
            })
            .max()
            .unwrap_or(1)
            .max(1);

        for i in 0..list_h {
            let idx = start + i;
            let Some(file) = self.files.get(idx) else {
                break;
            };
            let (a, d) = file.stats();
            let y = iy + i as u16;
            let selected = idx == self.selected;
            let marker = if selected { "▸" } else { " " };
            let name = shorten(file.path(), name_w);
            let name_style = if selected {
                on_bg.clone().bold()
            } else {
                on_bg.clone()
            };
            self.screen
                .set_str((ix, y), &format!("{marker}{name:<name_w$}"), name_style);
            let count = format!(" {:>count_w$} ", a + d);
            let cx = ix + 1 + name_w as u16;
            self.screen.set_str((cx, y), &count, on_bg.clone());
            let bx = cx + count.chars().count() as u16;
            let scaled = |n: usize| if n == 0 { 0 } else { ((n * bar_w) / max_count).max(1) };
            let ap = scaled(a);
            let dp = scaled(d).min(bar_w.saturating_sub(ap));
            self.screen
                .set_str((bx, y), &"+".repeat(ap), on_bg.clone().fg(base_fg(&self.theme.add)));
            self.screen.set_str(
                (bx + ap as u16, y),
                &"-".repeat(dp),
                on_bg.clone().fg(base_fg(&self.theme.remove)),
            );
        }

        // Summary line.
        let summary = format!(
            "{nf} files changed, {add} insertion(s)(+), {del} deletion(s)(-)",
        );
        self.screen.set_str(
            (ix, iy + inner_h - 1),
            &clip(&summary, inner_w),
            on_bg.bold(),
        );
    }

    fn render_diff(&mut self, x: u16, width: u16, body_h: u16) {
        // Move the cached rows out so we can freely borrow `self.screen`
        // while iterating; restored right after rendering.
        let rows = match self.row_cache.get_mut(self.selected).and_then(|o| o.take()) {
            Some(r) => r,
            None => return,
        };
        for row in 0..body_h {
            let idx = self.scroll + row as usize;
            let Some(r) = rows.get(idx) else {
                break;
            };
            let y = row;
            let is_cursor = idx == self.cursor && self.view == View::Diff;
            // Whole-line background: the cursor wins, otherwise added/removed
            // lines get a subtle wash (GitHub-style).
            let row_bg = if is_cursor {
                Some(self.theme.cursor_bg)
            } else {
                match r.kind {
                    RowKind::Add => self.theme.add_line_bg,
                    RowKind::Remove => self.theme.remove_line_bg,
                    _ => None,
                }
            };
            self.draw_diff_row(r, x, width, y, row_bg, Gut::Both);
        }
        // Restore the rows we borrowed.
        if let Some(slot) = self.row_cache.get_mut(self.selected) {
            *slot = Some(rows);
        }
    }

    /// Side-by-side rendering: old lines on the left pane, new on the right,
    /// context on both, with a divider column. Where one side has no matching
    /// line (a pure add or delete) that half is hatched with `╱`.
    fn render_split(&mut self, x: u16, width: u16, body_h: u16) {
        if width < 5 {
            return self.render_diff(x, width, body_h);
        }
        let left_w = (width - 1) / 2;
        let div_x = x + left_w;
        let right_x = div_x + 1;
        let right_w = width - left_w - 1;
        let rows = match self.row_cache.get_mut(self.selected).and_then(|o| o.take()) {
            Some(r) => r,
            None => return,
        };
        for row in 0..body_h {
            let idx = self.scroll + row as usize;
            let Some(r) = rows.get(idx) else {
                break;
            };
            let y = row;
            let is_cursor = idx == self.cursor && self.view == View::Diff;
            let cbg = if is_cursor {
                Some(self.theme.cursor_bg)
            } else {
                None
            };
            match r.kind {
                RowKind::Hunk | RowKind::Note => {
                    self.draw_diff_row(r, x, width, y, cbg, Gut::Both);
                    continue;
                }
                RowKind::Context => {
                    self.draw_diff_row(r, x, left_w, y, cbg, Gut::Old);
                    self.draw_diff_row(r, right_x, right_w, y, cbg, Gut::New);
                }
                RowKind::Remove => {
                    let lbg = cbg.or(self.theme.remove_line_bg);
                    self.draw_diff_row(r, x, left_w, y, lbg, Gut::Old);
                    self.fill_slash(right_x, right_w, y, cbg);
                }
                RowKind::Add => {
                    self.fill_slash(x, left_w, y, cbg);
                    let rbg = cbg.or(self.theme.add_line_bg);
                    self.draw_diff_row(r, right_x, right_w, y, rbg, Gut::New);
                }
            }
            let mut dv = self.theme.line_number.clone();
            if let Some(c) = cbg {
                dv = dv.bg(c);
            }
            self.screen.set_str((div_x, y), "│", dv);
        }
        if let Some(slot) = self.row_cache.get_mut(self.selected) {
            *slot = Some(rows);
        }
    }

    /// Hatch an empty pane half with diagonal slashes (used where a split-view
    /// line exists on only one side).
    fn fill_slash(&mut self, x: u16, width: u16, y: u16, bg: Option<Color>) {
        if width == 0 {
            return;
        }
        let mut st = self.theme.line_number.clone();
        if let Some(c) = bg {
            st = st.bg(c);
        }
        self.screen
            .set_str((x, y), &"╱".repeat(width as usize), st);
    }

    /// Draw a single diff row into the box `[x, x+width)` at screen row `y`,
    /// with an optional whole-row background wash and a gutter mode.
    fn draw_diff_row(&mut self, r: &Row, x: u16, width: u16, y: u16, row_bg: Option<Color>, gut: Gut) {
        if width == 0 {
            return;
        }
        let num_w: u16 = if !self.config.line_numbers {
            0
        } else {
            match gut {
                Gut::Both => 9,
                Gut::Old | Gut::New => 5,
            }
        };
        // Wash the whole row so gaps also carry the line/cursor background.
        if let Some(c) = row_bg {
            self.screen
                .set_str((x, y), &" ".repeat(width as usize), Style::default().bg(c));
        }
        // Apply the row background to a style that doesn't set its own.
        let bg = |st: Style| -> Style {
            match (row_bg, st.bg) {
                (Some(c), None) => st.bg(c),
                _ => st,
            }
        };
        let mut cx = x;

        // Gutter: line numbers.
        if self.config.line_numbers && matches!(r.kind, RowKind::Add | RowKind::Remove | RowKind::Context) {
            let onum = r.old_no.map(|n| n.to_string()).unwrap_or_default();
            let nnum = r.new_no.map(|n| n.to_string()).unwrap_or_default();
            let gutter = match gut {
                Gut::Both => format!("{onum:>4} {nnum:>4}"),
                Gut::Old => format!("{onum:>4} "),
                Gut::New => format!("{nnum:>4} "),
            };
            self.screen
                .set_str((cx, y), &gutter, bg(self.theme.line_number.clone()));
        }
        cx += num_w;

        // Sign column + base style per row kind.
        let (sign, base, sign_style) = match r.kind {
            RowKind::Add => ("+", &self.theme.add, self.theme.add.clone().bold()),
            RowKind::Remove => ("-", &self.theme.remove, self.theme.remove.clone().bold()),
            RowKind::Context => (" ", &self.theme.context, self.theme.context.clone()),
            RowKind::Hunk => {
                let s = clip(&r.spans[0].text, width);
                self.screen
                    .set_str((cx, y), &s, bg(self.theme.header.clone().faint()));
                return;
            }
            RowKind::Note => {
                let s = clip(&r.spans[0].text, width);
                self.screen
                    .set_str((cx, y), &s, bg(self.theme.context.clone().faint()));
                return;
            }
        };
        self.screen.set_str((cx, y), sign, bg(sign_style));
        cx += 1;

        let emph_bg = match r.kind {
            RowKind::Add => self.theme.add_emph_bg,
            RowKind::Remove => self.theme.remove_emph_bg,
            _ => None,
        };
        let avail = x + width;
        for span in &r.spans {
            if cx >= avail {
                break;
            }
            let remaining = (avail - cx) as usize;
            let text = clip(&span.text, remaining as u16);
            if text.is_empty() {
                continue;
            }
            let mut style = if self.config.syntax {
                Style::default().fg(span.fg.or_else(|| base_fg(base)))
            } else {
                base.clone()
            };
            if span.changed {
                if let Some(bgc) = emph_bg {
                    style = style.bg(bgc).bold();
                }
            }
            let cw = text.chars().count() as u16;
            self.screen.set_str((cx, y), &text, bg(style));
            cx += cw;
        }
    }

    pub fn finish(self) -> io::Result<()> {
        self.screen.finish()
    }
}

fn base_fg(style: &Style) -> Option<Color> {
    style.fg
}

/// Flatten a single file diff into styled display rows. Free-standing so it can
/// run on the startup prefetch thread as well as the lazy main-thread path.
fn build_file_rows(file: &FileDiff, hl: &Highlighter, intraline: bool) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    if file.is_binary {
        rows.push(Row {
            kind: RowKind::Note,
            old_no: None,
            new_no: None,
            spans: vec![Span {
                fg: None,
                changed: false,
                text: "Binary file — no textual diff".into(),
            }],
        });
        return rows;
    }

    for hunk in &file.hunks {
        rows.push(Row {
            kind: RowKind::Hunk,
            old_no: None,
            new_no: None,
            spans: vec![Span {
                fg: None,
                changed: false,
                text: hunk.header.clone(),
            }],
        });
        let mut fh = hl.file(file.path());
        for line in &hunk.lines {
            let syntax_spans = fh.line(&line.content);
            let spans = merge(syntax_spans, &line.segments, intraline);
            let kind = match line.kind {
                LineKind::Add => RowKind::Add,
                LineKind::Remove => RowKind::Remove,
                LineKind::Context => RowKind::Context,
            };
            rows.push(Row {
                kind,
                old_no: line.old_no,
                new_no: line.new_no,
                spans,
            });
        }
    }
    rows
}

/// Combine syntect color spans with intra-line changed segments into a single
/// span list carrying both foreground color and the changed flag.
fn merge(
    syntax: Vec<(Option<Color>, String)>,
    segments: &[diff::Segment],
    intraline: bool,
) -> Vec<Span> {
    if !intraline || segments.is_empty() {
        return syntax
            .into_iter()
            .map(|(fg, text)| Span {
                fg,
                changed: false,
                text,
            })
            .collect();
    }
    // Build a per-char changed mask from segments.
    let mut mask: Vec<bool> = Vec::new();
    for seg in segments {
        for _ in seg.text.chars() {
            mask.push(seg.changed);
        }
    }
    let mut out: Vec<Span> = Vec::new();
    let mut ci = 0usize;
    for (fg, text) in syntax {
        let mut cur = String::new();
        let mut cur_changed: Option<bool> = None;
        for ch in text.chars() {
            let changed = mask.get(ci).copied().unwrap_or(false);
            ci += 1;
            if cur_changed == Some(changed) {
                cur.push(ch);
            } else {
                if let Some(c) = cur_changed {
                    out.push(Span {
                        fg,
                        changed: c,
                        text: std::mem::take(&mut cur),
                    });
                }
                cur_changed = Some(changed);
                cur.push(ch);
            }
        }
        if let Some(c) = cur_changed {
            out.push(Span {
                fg,
                changed: c,
                text: cur,
            });
        }
    }
    out
}

fn clip(s: &str, width: u16) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = if (ch as u32) < 0x20 { 0 } else { 1 };
        if w + cw > width as usize {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

/// Shorten a path to fit, keeping the tail (filename) visible.
fn shorten(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let tail: String = s.chars().rev().take(width.saturating_sub(1)).collect();
    let tail: String = tail.chars().rev().collect();
    format!("…{tail}")
}
