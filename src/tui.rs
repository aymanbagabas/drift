//! The terminal UI: a scrollable, syntax-highlighted diff pane with a single
//! bottom footer, driven by an uncurses `Screen`.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use uncurses::buffer::{Bounded, Line, SurfaceMut};
use uncurses::cell::Cell;
use uncurses::color::Color;
use uncurses::event::{Event, MouseButton};
use uncurses::screen::{MouseTracking, Screen, ScreenOptions};
use uncurses::style::Style;
use uncurses::terminal::{TtyInput, TtyOutput};
use uncurses::text::{grapheme_cells, TextSurface, WidthMode};

use crate::config::{parse_style, Config, Palette};
use crate::diff::{self, FileDiff, LineKind};

/// How long a transient footer note stays before it auto-expires.
const FLASH: Duration = Duration::from_secs(2);
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
    /// Spanning background band behind a hunk header, so it reads as a section
    /// separator; a subtle blue-tinted tone distinct from added/removed washes.
    header_bg: Option<Color>,
    cursor_bg: Color,
    // Terminal default background (OSC 11); `None` rides the terminal's own.
    background: Option<Color>,
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
    // Sidebar.
    sidebar_border: Style,
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
            line_number: sty("line-number", "line-number"),
            add_emph_bg: pal.color("add-emph"),
            remove_emph_bg: pal.color("remove-emph"),
            add_line_bg: pal.color("add-line"),
            remove_line_bg: pal.color("remove-line"),
            header_bg: pal.color("header-line"),
            cursor_bg: pal.color("cursor").unwrap_or(Color::Indexed(237)),
            background: pal.color("background"),
            statusbar: sty("statusbar", "foreground surface"),
            statusbar_logo: sty("statusbar-logo", "background primary bold"),
            statusbar_filename: sty("statusbar-filename", "foreground surface bold"),
            statusbar_add: sty("statusbar-add", "add surface"),
            statusbar_remove: sty("statusbar-remove", "remove surface"),
            statusbar_flags: sty("statusbar-flags", "muted surface"),
            statusbar_stats: sty("statusbar-stats", "foreground surface"),
            statusbar_watch: sty("statusbar-watch", "background add bold"),
            statusbar_help: sty("statusbar-help", "background secondary bold"),
            help_key: sty("help-key", "muted bold"),
            help_desc: sty("help-desc", "muted faint"),
            dialog: sty("dialog", "foreground background"),
            dialog_border: sty("dialog-border", "surface"),
            sidebar_border: sty("sidebar-border", "surface"),
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

/// Which split pane a selection lives in. Selection is confined to one pane at
/// a time since the two sides have independent reading orders.
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Left,
    Right,
}

struct Row {
    kind: RowKind,
    old_no: Option<usize>,
    new_no: Option<usize>,
    spans: Vec<Span>,
    /// The row's content parsed into display cells once, so selection can read
    /// exact column-to-text mapping (wide chars included) without touching the
    /// screen. A cell index equals a screen column offset from `content_start`.
    content: Line,
}

impl Row {
    fn new(kind: RowKind, old_no: Option<usize>, new_no: Option<usize>, spans: Vec<Span>) -> Self {
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        let content = text_cells(&text);
        Row {
            kind,
            old_no,
            new_no,
            spans,
            content,
        }
    }
}

/// A mouse text selection anchored to the content model (not the screen), so
/// it survives scrolling and can span more rows than fit on screen. `a_*` is
/// where the drag began, `c_*` follows the pointer. `col` is a **cell** index
/// into the row's content cells (span text only: no gutter, no +/- sign).
#[derive(Clone, Copy)]
struct Sel {
    a_row: usize,
    a_col: usize,
    c_row: usize,
    c_col: usize,
    dragging: bool,
    /// Which split pane the selection is confined to; `None` in unified view.
    pane: Option<Pane>,
}

impl Sel {
    /// (start_row, start_col, end_row, end_col) in reading order.
    fn ordered(&self) -> (usize, usize, usize, usize) {
        if (self.a_row, self.a_col) <= (self.c_row, self.c_col) {
            (self.a_row, self.a_col, self.c_row, self.c_col)
        } else {
            (self.c_row, self.c_col, self.a_row, self.a_col)
        }
    }

    fn is_empty(&self) -> bool {
        self.a_row == self.c_row && self.a_col == self.c_col
    }
}

/// Parse a string into display cells, inserting a continuation cell after each
/// wide grapheme so `cells.len()` equals the string's width in terminal columns.
/// That makes a cell index equal to a screen column, matching how the renderer
/// lays the same text out. Selection reads these instead of the screen buffer,
/// so it works even when the selection is taller than the viewport.
fn text_cells(s: &str) -> Line {
    let mut cells = Line::new();
    for (g, w) in grapheme_cells(s, WidthMode::Grapheme, false) {
        if w >= 2 {
            cells.push(Cell::wide(g));
            // One continuation cell per extra column, so `cells.len()` equals
            // the grapheme's display width for any wide cluster.
            for _ in 1..w {
                cells.push(Cell::continuation());
            }
        } else if w == 1 {
            cells.push(Cell::narrow(g));
        }
    }
    cells
}

/// Join cells `[start, end)` into a string, trimming trailing blanks the way a
/// terminal copy does. Continuation cells contribute "" so a wide char appears
/// exactly once. Indices are clamped to the cell slice.
fn slice_cells(cells: &[Cell], start: usize, end: usize) -> String {
    let s = start.min(cells.len());
    let e = end.min(cells.len()).max(s);
    let mut line: String = cells[s..e].iter().map(|c| c.content()).collect();
    while line.ends_with(' ') {
        line.pop();
    }
    line
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
    /// The raw unified-diff text for each file, in the same order as `files`,
    /// so `Y` can copy an exact per-file patch without reconstructing it.
    raw_files: Vec<String>,
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
    /// Left file-list sidebar override: `None` follows the auto rule (open when
    /// the terminal is >= 150 cells wide), `Some(_)` is the user's toggle.
    sidebar: Option<bool>,
    /// Runtime sidebar width override (cells) from a mouse-drag resize; `None`
    /// follows `config.sidebar_width`.
    sidebar_width: Option<usize>,
    /// True while dragging the sidebar divider to resize it.
    resizing: bool,
    /// True while dragging the split-view divider to resize the two panes.
    resizing_split: bool,
    /// Last body left-click (time, x, y) for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
    /// Side-by-side (split) diff rendering, toggled with `s`.
    split: bool,
    /// Left pane's fraction of the split body (drag the divider to change it).
    split_ratio: f32,
    help_open: bool,
    /// Screen x where the "? help" footer badge starts, for click-to-toggle.
    help_badge_x: u16,
    /// Clickable geometry of the stat modal from the last render:
    /// `(box_x0, box_y0, box_x1, box_y1, list_y0, list_h, start)`. Lets a click
    /// map to a file row, and tells inside-the-box clicks from outside ones.
    modal_hit: Option<(u16, u16, u16, u16, u16, usize, usize)>,
    /// Top-left of the stat modal, once dragged; `None` = auto-centered.
    modal_pos: Option<(u16, u16)>,
    /// While dragging the modal: the grab offset into the box `(dx, dy)`.
    modal_drag: Option<(u16, u16)>,
    /// Last window title pushed to the terminal, to avoid redundant writes.
    title: String,
    /// Whether watch mode reacts to git changes (toggle with `w`).
    watch: bool,
    /// Extra lines of context added on top of the base setting, grown by
    /// expanding folded regions with Enter on a hunk header.
    expand: usize,
    /// Active mouse text selection, drawn reversed and yanked with `y`.
    sel: Option<Sel>,
    /// Transient footer note (e.g. "copied 3 lines") with the instant it was
    /// set, so it auto-expires after `FLASH`.
    flash: Option<(String, Instant)>,
    /// Raw diff text last applied, so the worktree poll only rebuilds when the
    /// unstaged diff actually changed (avoids jarring scroll resets on idle).
    last_diff: String,
}

impl App {
    pub fn new(config: Config, source: Source, opts: crate::git::Opts) -> io::Result<Self> {
        let highlighter = Arc::new(Highlighter::new(&config.theme, config.syntax_enabled()));
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
        // Paint the whole terminal in the theme's background so unwritten gaps
        // match the diff body. Skipped when the theme rides the terminal's own
        // background (e.g. the `ansi` theme). uncurses resets this on finish().
        if let Some(c) = theme.background {
            screen.set_background_color(c)?;
        }
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
            raw_files: Vec::new(),
            row_cache: Vec::new(),
            prefetch: None,
            selected: 0,
            scroll: 0,
            cursor: 0,
            view: View::Diff,
            sidebar: None,
            sidebar_width: None,
            resizing: false,
            resizing_split: false,
            last_click: None,
            split: false,
            split_ratio: 0.5,
            help_open: false,
            help_badge_x: 0,
            modal_hit: None,
            modal_pos: None,
            modal_drag: None,
            title: String::new(),
            watch: false,
            expand: 0,
            sel: None,
            flash: None,
            last_diff: String::new(),
        };
        app.start();
        Ok(app)
    }

    /// Initial load used at startup: run the diff, parse it, then hand row
    /// building off to a background thread so the first frame paints without
    /// blocking on syntax highlighting.
    fn start(&mut self) {
        match self.source.diff(&self.effective_opts()) {
            Ok(text) => {
                self.files = diff::parse(&text);
                self.raw_files = diff::split_files(&text);
                self.last_diff = text;
            }
            Err(_) => {
                self.files.clear();
                self.raw_files.clear();
                self.last_diff.clear();
            }
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
        if self.files.is_empty() {
            self.prefetch = None;
            return;
        }
        let files = Arc::new(self.files.clone());
        let hl = Arc::clone(&self.highlighter);
        let intraline = self.config.intraline_enabled();
        let tab = self.config.tab_width;
        let sel = self.selected;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let n = files.len();
            let order = std::iter::once(sel).chain((0..n).filter(|&i| i != sel));
            for idx in order {
                let rows = build_file_rows(&files[idx], &hl, intraline, tab);
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
        let text = self.source.diff(&self.effective_opts()).unwrap_or_default();
        self.rebuild_from(text);
    }

    /// Rebuild the view from already-fetched diff text, preserving the cursor
    /// position. Shared by `reload` and the worktree poll (which passes the
    /// text it just fetched to compare, avoiding a second git call).
    fn rebuild_from(&mut self, text: String) {
        let (sel, cur) = (self.selected, self.cursor);
        // Drop any in-flight startup prefetch so its (now stale) rows can't
        // land in the freshly rebuilt cache.
        self.prefetch = None;
        self.files = diff::parse(&text);
        self.raw_files = diff::split_files(&text);
        self.last_diff = text;
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
            Some(file) => build_file_rows(
                file,
                &self.highlighter,
                self.config.intraline_enabled(),
                self.config.tab_width,
            ),
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
            ("b", "sidebar"),
            ("w", "watch on/off"),
            ("enter", "expand context"),
            ("e", "edit in $EDITOR"),
            ("y", "copy line/selection"),
            ("Y", "copy file diff"),
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
            .map(|(k, v)| self.width(k) as usize + 2 + self.width(v) as usize)
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

    /// Scroll the viewport by `delta` rows without moving the cursor, clamped to
    /// the content. Used by drag-select to auto-scroll past the visible edge.
    fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll() as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    /// Width of the line-number gutter for a pane, matching what
    /// [`Self::draw_diff_row`] draws.
    fn gutter_w(&self, gut: Gut) -> u16 {
        if !self.config.line_numbers {
            0
        } else {
            match gut {
                Gut::Both => 9,
                Gut::Old | Gut::New => 5,
            }
        }
    }

    /// Whether the file-list sidebar is showing: the user's runtime toggle
    /// wins, otherwise the `sidebar` config mode decides ("always", "never", or
    /// "auto" = open on terminals at least 150 cells wide, roomy enough to keep
    /// the diff body comfortable next to a 30-cell sidebar).
    fn sidebar_visible(&self) -> bool {
        if self.files.is_empty() {
            return false;
        }
        self.sidebar.unwrap_or_else(|| match self.config.sidebar.as_str() {
            "always" | "on" | "open" => true,
            "never" | "off" | "closed" => false,
            _ => self.screen.width() >= 150,
        })
    }

    /// Sidebar width in cells (including its 1-cell divider), 0 when hidden.
    /// Clamped so it never eats more than half the terminal.
    fn sidebar_w(&self) -> u16 {
        if !self.sidebar_visible() {
            return 0;
        }
        let max = (self.screen.width() / 2).max(2);
        let want = self.sidebar_width.unwrap_or(self.config.sidebar_width);
        (want as u16).clamp(8, max)
    }

    /// Screen column of the sidebar's resize divider (the edge facing the body).
    fn divider_x(&self) -> u16 {
        let sw = self.sidebar_w();
        if self.sidebar_left() {
            sw.saturating_sub(1)
        } else {
            self.screen.width().saturating_sub(sw)
        }
    }

    /// Resize the sidebar so its divider follows screen column `x`.
    fn resize_sidebar_to(&mut self, x: u16) {
        let w = self.screen.width();
        let width = if self.sidebar_left() {
            x + 1
        } else {
            w.saturating_sub(x)
        };
        let max = (w / 2).max(2);
        self.sidebar_width = Some((width as usize).clamp(8, max as usize));
    }

    /// Left pane width (cells) of a split body of total `width`, honoring the
    /// drag ratio and leaving at least 2 cells on each side of the divider.
    fn split_left_w(&self, width: u16) -> u16 {
        let inner = width.saturating_sub(1);
        ((inner as f32 * self.split_ratio).round() as u16).clamp(2, inner.saturating_sub(2))
    }

    /// Screen column of the split divider, for click-to-drag hit testing.
    fn split_div_x(&self) -> u16 {
        let bw = self.screen.width().saturating_sub(self.sidebar_w());
        self.body_x() + self.split_left_w(bw)
    }

    /// Resize the split so its divider follows screen column `x`.
    fn resize_split_to(&mut self, x: u16) {
        let bx = self.body_x();
        let inner = self
            .screen
            .width()
            .saturating_sub(self.sidebar_w())
            .saturating_sub(1);
        if inner < 4 {
            return;
        }
        let left = x.saturating_sub(bx).clamp(2, inner - 2);
        self.split_ratio = left as f32 / inner as f32;
    }

    /// True when the sidebar sits on the left (default), false for the right.
    fn sidebar_left(&self) -> bool {
        self.config.sidebar_side != "right"
    }

    /// Screen x where the diff body begins (right of a left sidebar).
    fn body_x(&self) -> u16 {
        if self.sidebar_left() {
            self.sidebar_w()
        } else {
            0
        }
    }

    /// True when screen column `x` falls inside the sidebar (either side).
    fn in_sidebar(&self, x: u16) -> bool {
        let sw = self.sidebar_w();
        if sw == 0 {
            return false;
        }
        if self.sidebar_left() {
            x < sw
        } else {
            x >= self.screen.width().saturating_sub(sw)
        }
    }

    /// Scroll offset of a file list `list_h` rows tall, keeping `selected`
    /// visible (matches the modal's window so click mapping lines up).
    fn file_window(&self, list_h: usize) -> usize {
        self.selected.saturating_sub(list_h.saturating_sub(1))
    }

    /// Screen column, within a pane, where a row's content begins: after the
    /// gutter and the one-column +/- sign (hunk/note rows have no sign).
    fn content_start(&self, kind: RowKind, gut: Gut) -> u16 {
        let sign = if matches!(kind, RowKind::Add | RowKind::Remove | RowKind::Context) {
            1
        } else {
            0
        };
        self.gutter_w(gut) + sign
    }

    /// Map a pointer at screen (x, y) to a (row, content-column) position.
    /// `pane` selects the split half (its gutter side and screen origin); in
    /// unified view pass `None`. Column is a cell index into the row's content.
    fn point_to_content(&self, x: u16, y: u16, pane: Option<Pane>) -> (usize, usize) {
        let rows = self.rows();
        if rows.is_empty() {
            return (0, 0);
        }
        let row = (self.scroll + y as usize).min(rows.len() - 1);
        let (origin, cs) = self.pane_geom(rows[row].kind, pane);
        let len = rows[row].content.len();
        let col = (x.saturating_sub(origin + cs) as usize).min(len);
        (row, col)
    }

    /// Which split pane screen column `x` falls in (left/right of the divider).
    /// The divider column itself belongs to neither (it's a resize grab).
    fn pane_at(&self, x: u16) -> Option<Pane> {
        let div = self.split_div_x();
        if x < div {
            Some(Pane::Left)
        } else if x > div {
            Some(Pane::Right)
        } else {
            None
        }
    }

    /// (screen origin x, content_start) for a row of `kind` under a selection
    /// pane. Hunk/Note headers span the full body, so they always anchor at the
    /// body origin regardless of pane.
    fn pane_geom(&self, kind: RowKind, pane: Option<Pane>) -> (u16, u16) {
        let bx = self.body_x();
        match pane {
            None => (bx, self.content_start(kind, Gut::Both)),
            Some(_) if matches!(kind, RowKind::Hunk | RowKind::Note) => {
                (bx, self.content_start(kind, Gut::Both))
            }
            Some(Pane::Left) => (bx, self.content_start(kind, Gut::Old)),
            Some(Pane::Right) => {
                let bw = self.screen.width().saturating_sub(self.sidebar_w());
                (bx + self.split_left_w(bw) + 1, self.content_start(kind, Gut::New))
            }
        }
    }

    /// Whether a row of `kind` has content in the selection `pane`. The left
    /// pane holds context + removals, the right holds context + additions;
    /// headers belong to both. In unified view every row qualifies.
    fn row_in_pane(kind: RowKind, pane: Option<Pane>) -> bool {
        match pane {
            None => true,
            Some(Pane::Left) => matches!(
                kind,
                RowKind::Context | RowKind::Remove | RowKind::Hunk | RowKind::Note
            ),
            Some(Pane::Right) => matches!(
                kind,
                RowKind::Context | RowKind::Add | RowKind::Hunk | RowKind::Note
            ),
        }
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

    pub fn run(
        &mut self,
        refresh: Option<Receiver<()>>,
        watch: bool,
        poll: Duration,
    ) -> io::Result<()> {
        self.watch = watch;
        let mut last_poll = Instant::now();
        loop {
            // Fold in any rows the startup worker has finished.
            self.drain_prefetch();
            // Expire a transient footer note after its lifetime.
            if self.flash.as_ref().is_some_and(|(_, t)| t.elapsed() >= FLASH) {
                self.flash = None;
            }
            // Catch unstaged edits the git-internals watcher can't see.
            if last_poll.elapsed() >= poll {
                self.poll_worktree();
                last_poll = Instant::now();
            }
            self.render()?;
            // Poll faster while the prefetch is still streaming so freshly
            // parsed files appear promptly; idle at 200ms once it's done.
            let mut timeout = if self.prefetch.is_some() {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(200)
            };
            // While a note is showing, wake around its expiry so it clears on
            // time rather than on the next idle tick.
            if let Some((_, t)) = &self.flash {
                timeout = timeout.min(FLASH.saturating_sub(t.elapsed()).max(Duration::from_millis(1)));
            }
            // Don't oversleep past the next worktree poll when watching.
            if self.watch && self.source.reads_worktree() {
                timeout = timeout.min(poll.max(Duration::from_millis(1)));
            }
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

    /// Poll fallback for unstaged working-tree edits, which don't touch the
    /// index or refs and so escape the git-internals watcher. Re-runs the diff
    /// and rebuilds only when the text actually changed, so idle ticks with no
    /// edits are free. Only active for worktree sources in watch mode.
    fn poll_worktree(&mut self) {
        if !self.watch || !self.source.reads_worktree() {
            return;
        }
        if let Ok(text) = self.source.diff(&self.effective_opts()) {
            if text != self.last_diff {
                self.rebuild_from(text);
            }
        }
    }

    /// Handle one event. Returns Ok(true) when the app should quit.
    fn handle(&mut self, ev: Event) -> io::Result<bool> {
        let page = self.viewport_rows() as isize;
        match ev {
            Event::KeyPress(k) => {
                // Escape closes transient UI in priority order: an active
                // selection first, then the stat modal. It never quits and
                // never touches help (help is a toggle-only inline footer,
                // closed with `?`).
                if k.matches("escape") {
                    if self.sel.is_some() {
                        self.sel = None;
                    } else if self.view == View::Stat {
                        self.view = View::Diff;
                    }
                    return Ok(false);
                }
                // Match with the uncurses key matcher: `matches` compares the
                // produced glyph (so shifted symbols like `}` and uppercase
                // synonyms work) and falls back to named-key patterns.
                // The help grid is an inline footer, not a blocking overlay:
                // `?` toggles it off, every other key still works normally.
                if self.help_open {
                    if k.matches_any(["q", "ctrl+c"]) {
                        return Ok(true);
                    }
                    if k.matches("?") {
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
                        self.select_file_at(0);
                    } else if k.matches_any(["G", "end"]) {
                        self.select_file_at(self.files.len().saturating_sub(1));
                    } else if k.matches_any(["f", "tab"]) {
                        self.view = View::Diff;
                    } else if k.matches("?") {
                        self.help_open = !self.help_open;
                    } else if k.matches_any(["enter", "e"]) {
                        self.view = View::Diff;
                        self.ensure_rows();
                        self.cursor_to(0);
                    } else if k.matches("r") {
                        self.reload();
                    } else if k.matches("Y") {
                        self.yank_file()?;
                    }
                    return Ok(false);
                }
                if k.matches_any(["q", "ctrl+c"]) {
                    return Ok(true);
                } else if k.matches("y") {
                    self.yank()?;
                    return Ok(false);
                } else if k.matches("Y") {
                    self.yank_file()?;
                    return Ok(false);
                }
                // Any other navigation key cancels an active selection; the
                // "copied" note fades on its own timer.
                self.sel = None;
                if k.matches_any(["j", "down"]) {
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
                } else if k.matches("b") {
                    self.sidebar = Some(!self.sidebar_visible());
                } else if k.matches("w") {
                    self.watch = !self.watch;
                } else if k.matches("?") {
                    self.help_open = !self.help_open;
                } else if k.matches("e") {
                    self.open_editor()?;
                } else if k.matches("enter") {
                    // Enter expands folded context on a hunk header; it no
                    // longer opens the editor (use `e` for that).
                    if self.on_hunk() && !matches!(self.source, Source::Stdin) {
                        self.expand_here();
                    }
                } else if k.matches("r") {
                    self.reload();
                }
            }
            Event::MouseWheel(m) => {
                // Scrolling moves content under any selection, so drop it.
                self.sel = None;
                if m.button == MouseButton::WheelUp {
                    self.move_cursor(-3);
                } else if m.button == MouseButton::WheelDown {
                    self.move_cursor(3);
                }
            }
            Event::MouseClick(m) => {
                if m.button != MouseButton::Left {
                    return Ok(false);
                }
                let footer_row = self.viewport_rows() as u16;
                // The "? help" badge toggles the help grid from any view.
                if m.y == footer_row && m.x >= self.help_badge_x {
                    self.help_open = !self.help_open;
                    return Ok(false);
                }
                // Stat modal: a click inside the box keeps it open (and selects
                // the file row under the cursor, if any); only a click outside
                // the box closes it.
                if self.view == View::Stat {
                    if let Some((bx0, by0, bx1, by1, ly0, list_h, start)) = self.modal_hit {
                        if m.x >= bx0 && m.x < bx1 && m.y >= by0 && m.y < by1 {
                            if m.y >= ly0 && m.y < ly0 + list_h as u16 {
                                let idx = start + (m.y - ly0) as usize;
                                if idx < self.files.len() {
                                    self.select_file_at(idx);
                                }
                            } else {
                                // Grab the border/summary area to drag the modal.
                                self.modal_drag = Some((m.x - bx0, m.y - by0));
                            }
                            return Ok(false);
                        }
                    }
                    self.view = View::Diff;
                    return Ok(false);
                }
                if self.view == View::Diff && !self.help_open {
                    let body_h = footer_row;
                    if m.y >= body_h {
                        // Footer area (not the help badge): ignore.
                    } else if self.sidebar_w() > 0 && m.x == self.divider_x() {
                        // Grab the divider to resize the sidebar.
                        self.resizing = true;
                    } else if self.split && m.x == self.split_div_x() {
                        // Grab the split divider to resize the two panes.
                        self.resizing_split = true;
                    } else if self.in_sidebar(m.x) {
                        // Click a file in the sidebar.
                        let idx = self.file_window(body_h as usize) + m.y as usize;
                        self.select_file_at(idx);
                    } else {
                        self.set_cursor(self.scroll + m.y as usize);
                        // Detect a double-click (same cell, quick succession):
                        // on a hunk header it expands the folded context.
                        let now = Instant::now();
                        let dbl = self.last_click.is_some_and(|(t, lx, ly)| {
                            ly == m.y
                                && lx.abs_diff(m.x) <= 1
                                && now.duration_since(t) < Duration::from_millis(400)
                        });
                        self.last_click = Some((now, m.x, m.y));
                        if dbl && self.on_hunk() && !matches!(self.source, Source::Stdin) {
                            self.sel = None;
                            self.last_click = None;
                            self.expand_here();
                        } else {
                            // Selection is confined to one pane in split view
                            // (each side has its own reading order); `None` in
                            // the unified view spans the whole width.
                            let pane = if self.split { self.pane_at(m.x) } else { None };
                            let (r, c) = self.point_to_content(m.x, m.y, pane);
                            self.sel = Some(Sel {
                                a_row: r,
                                a_col: c,
                                c_row: r,
                                c_col: c,
                                dragging: true,
                                pane,
                            });
                        }
                    }
                }
            }
            Event::MouseMove(m) => {
                // With button tracking (mode 1002) motion is only reported
                // while a button is held, so any move during a drag extends the
                // selection.
                if let Some((gx, gy)) = self.modal_drag {
                    self.modal_pos = Some((m.x.saturating_sub(gx), m.y.saturating_sub(gy)));
                } else if self.resizing {
                    self.resize_sidebar_to(m.x);
                } else if self.resizing_split {
                    self.resize_split_to(m.x);
                } else if self.sel.is_some_and(|s| s.dragging) {
                    let body_h = self.viewport_rows() as u16;
                    // Dragging past the top/bottom edge scrolls, so a selection
                    // can grow beyond the visible rows.
                    if m.y == 0 {
                        self.scroll_by(-1);
                    } else if m.y + 1 >= body_h {
                        self.scroll_by(1);
                    }
                    let y = m.y.min(body_h.saturating_sub(1));
                    let pane = self.sel.and_then(|s| s.pane);
                    let (r, c) = self.point_to_content(m.x, y, pane);
                    if let Some(sel) = self.sel.as_mut() {
                        sel.c_row = r;
                        sel.c_col = c;
                    }
                }
            }
            Event::MouseRelease(m) => {
                if m.button == MouseButton::Left {
                    self.resizing = false;
                    self.resizing_split = false;
                    self.modal_drag = None;
                    if let Some(sel) = self.sel.as_mut() {
                        sel.dragging = false;
                        // A click with no drag selects nothing.
                        if sel.is_empty() {
                            self.sel = None;
                        }
                    }
                }
            }
            Event::Resize(ws) => {
                self.sel = None;
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

    /// Copy to the system clipboard via the terminal's OSC 52 (uncurses
    /// `set_system_clipboard`), so it works over SSH with no external clipboard
    /// tool. With a mouse selection active it copies that; with none it copies
    /// the whole line under the cursor. The text comes from the row model, not
    /// the screen, so a selection taller than the viewport still copies in full,
    /// without the line-number gutter or the +/- signs.
    fn yank(&mut self) -> io::Result<()> {
        let text = match self.sel {
            Some(sel) => {
                self.sel = None;
                self.selection_text(sel)
            }
            // No selection: copy the whole line under the cursor.
            None => {
                let rows = self.rows();
                match rows.get(self.cursor) {
                    Some(row) => slice_cells(&row.content, 0, usize::MAX),
                    None => return Ok(()),
                }
            }
        };
        if text.is_empty() {
            return Ok(());
        }
        let lines = text.matches('\n').count() + 1;
        self.screen.set_system_clipboard(text.as_bytes())?;
        self.set_flash(format!(
            "copied {} line{}",
            lines,
            if lines == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    /// Set a transient footer note stamped with the current time.
    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    /// Copy the selected file's raw unified diff (exactly as git produced it)
    /// to the system clipboard.
    fn yank_file(&mut self) -> io::Result<()> {
        let Some(patch) = self.raw_files.get(self.selected) else {
            return Ok(());
        };
        if patch.is_empty() {
            return Ok(());
        }
        self.screen.set_system_clipboard(patch.as_bytes())?;
        self.set_flash("copied file diff");
        Ok(())
    }
    fn selection_text(&self, sel: Sel) -> String {
        let rows = self.rows();
        if rows.is_empty() {
            return String::new();
        }
        let (sr, sc, er, ec) = sel.ordered();
        let er = er.min(rows.len() - 1);
        let mut lines = Vec::new();
        for r in sr..=er {
            // In split view only the pane's own rows contribute, so the copied
            // text is one clean side (old or new), not an interleave.
            if !App::row_in_pane(rows[r].kind, sel.pane) {
                continue;
            }
            let cells = &rows[r].content;
            let start = if r == sr { sc } else { 0 };
            let end = if r == er { ec } else { usize::MAX };
            lines.push(slice_cells(cells, start, end));
        }
        lines.join("\n")
    }

    /// Reverse-video the selected content cells of the visible rows in the
    /// freshly drawn frame.
    fn paint_selection(&mut self, sel: Sel) {
        if self.rows().is_empty() {
            return;
        }
        let w = self.screen.width();
        let sw = self.sidebar_w();
        // Clamp highlights to the diff body so they never spill into a sidebar
        // (right edge is the terminal width minus the sidebar); in split view a
        // left-pane highlight also stops at the divider.
        let body_right = if self.sidebar_left() { w } else { w - sw };
        let right = match sel.pane {
            Some(Pane::Left) => self.split_div_x().min(body_right),
            _ => body_right,
        };
        let body_h = self.viewport_rows();
        let scroll = self.scroll;
        let (sr, sc, er, ec) = sel.ordered();
        // Compute the on-screen highlight span for each visible selected row up
        // front, so the immutable row borrow is released before we touch cells.
        let mut segs: Vec<(u16, u16, u16)> = Vec::new();
        {
            let rows = self.rows();
            let er = er.min(rows.len().saturating_sub(1));
            for r in sr..=er {
                if r < scroll || r >= scroll + body_h {
                    continue;
                }
                let row = &rows[r];
                if !App::row_in_pane(row.kind, sel.pane) {
                    continue;
                }
                let (origin, cstart) = self.pane_geom(row.kind, sel.pane);
                let cs = origin + cstart;
                let len = row.content.len() as u16;
                let start = if r == sr { sc as u16 } else { 0 };
                let end = if r == er { ec as u16 } else { len };
                let sx = (cs + start.min(len)).min(right);
                let ex = (cs + end.min(len)).min(right);
                if ex > sx {
                    segs.push(((r - scroll) as u16, sx, ex));
                }
            }
        }
        for (y, sx, ex) in segs {
            for x in sx..ex {
                if let Some(c) = self.screen.cell_mut((x, y)) {
                    c.style = c.style.clone().reverse();
                }
            }
        }
    }

    fn update_title(&mut self) -> io::Result<()> {
        let want = match self.files.get(self.selected) {
            Some(f) => format!("{} · diffv", f.path()),
            None => "diffv".to_string(),
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

        // The diff body fills the width left over by the sidebar; the sidebar
        // sits on the configured side, spanning the body height.
        let sw = self.sidebar_w();
        let bx = self.body_x();
        let bw = w.saturating_sub(sw);
        if self.split {
            self.render_split(bx, bw, body_h);
        } else {
            self.render_diff(bx, bw, body_h);
        }
        if sw > 0 {
            let sx = if self.sidebar_left() { 0 } else { w - sw };
            self.render_sidebar(sx, sw, body_h);
        }

        // Footer bar sits just above the help grid (when the grid is open the
        // footer is "pushed up" to make room below it).
        let footer_row = body_h;
        self.render_footer(footer_row);
        if self.help_open {
            self.render_help_grid(footer_row + 1, h);
        }
        // The stat modal floats above everything, footer and help included, so
        // it must be drawn last.
        if self.view == View::Stat {
            self.render_stat_modal();
        }
        // Overlay the selection highlight on top of the finished frame.
        if let Some(sel) = self.sel {
            self.paint_selection(sel);
        }
        self.screen.render()
    }

    /// The single bottom footer: a bold "diffv" badge, the current file name,
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
        let help_x = w.saturating_sub(self.width(help_badge));
        self.help_badge_x = help_x;
        self.screen
            .set_str((help_x, row), help_badge, self.theme.statusbar_help.clone());

        let w_x = if self.watch {
            let w_badge = " W ";
            let wx = help_x.saturating_sub(self.width(w_badge));
            self.screen
                .set_str((wx, row), w_badge, self.theme.statusbar_watch.clone());
            wx
        } else {
            help_x
        };

        // Global stats.
        let stats = format!(" {nf} files +{add} -{del} ");
        let stats_x = w_x.saturating_sub(self.width(&stats));
        self.screen
            .set_str((stats_x, row), &stats, self.theme.statusbar_stats.clone());

        // Left: bold "diffv" badge in the primary accent.
        let app = " diffv ";
        self.screen
            .set_str((0, row), app, self.theme.statusbar_logo.clone());
        let mut x = self.width(app);

        // File name.
        let name_seg = format!(" {name}");
        let (name_clip, name_w) = self.clip(&name_seg, stats_x.saturating_sub(x));
        self.screen
            .set_str((x, row), &name_clip, self.theme.statusbar_filename.clone());
        x += name_w;

        // Per-file line stats and flags, a muted group next to the file name.
        if let Some(f) = file {
            let (fa, fd) = f.stats();
            let put = |s: &mut Self, x: &mut u16, text: &str, style: Style| {
                if *x >= stats_x {
                    return;
                }
                let (t, tw) = s.clip(text, stats_x - *x);
                s.screen.set_str((*x, row), &t, style);
                *x += tw;
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

        // A transient note (e.g. "copied 3 lines") sits right after the
        // per-file stats and auto-expires; clip it to the space left before the
        // right-aligned global stats.
        if let Some((msg, _)) = self.flash.clone() {
            if x < stats_x {
                let badge = format!(" {msg} ");
                let (badge, _) = self.clip(&badge, stats_x - x);
                self.screen
                    .set_str((x, row), &badge, self.theme.statusbar_add.clone());
            }
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
        let key_w = entries.iter().map(|(k, _)| self.width(k)).max().unwrap_or(0);
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
        // Follow the dragged position (clamped on-screen), else center.
        let (x0, y0) = match self.modal_pos {
            Some((px, py)) => (px.min(w - bw), py.min(h - bh)),
            None => ((w - bw) / 2, (h - bh) / 2),
        };
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

    /// Draw the file-list sidebar in `[sx, sx+sw)` for rows `[0, height)`: a
    /// scrollable list of file names with +/- counts, plus a divider column on
    /// the edge facing the diff body.
    fn render_sidebar(&mut self, sx: u16, sw: u16, height: u16) {
        if sw < 2 {
            return;
        }
        let left = self.sidebar_left();
        // Divider hugs the body: right edge for a left sidebar, left edge for a
        // right one. The list fills the remaining columns.
        let (div_x, list_x) = if left { (sx + sw - 1, sx) } else { (sx, sx + 1) };
        let list_w = sw - 1;
        let start = self.file_window(height as usize);
        let border = self.theme.sidebar_border.clone();
        for row in 0..height {
            let idx = start + row as usize;
            self.draw_file_entry(list_x, row, list_w, idx);
            self.screen.set_str((div_x, row), "│", border.clone());
        }
    }

    /// Draw one file entry (marker, name, right-aligned +/- counts) filling the
    /// row `[x, x+w)` on the sidebar surface.
    fn draw_file_entry(&mut self, x: u16, y: u16, w: u16, idx: usize) {
        let surface = self.theme.dialog.clone();
        self.screen
            .set_str((x, y), &" ".repeat(w as usize), surface.clone());
        let Some(file) = self.files.get(idx) else {
            return;
        };
        let (a, d) = file.stats();
        let selected = idx == self.selected;
        let marker = if selected { "▸ " } else { "  " };
        let count = format!(" +{a} -{d} ");
        let cw = self.width(&count).min(w);
        let name_w = w.saturating_sub(2 + cw);
        let name = self.shorten(file.path(), name_w);
        let style = if selected {
            surface.clone().bold()
        } else {
            surface.clone()
        };
        self.screen.set_str((x, y), &format!("{marker}{name}"), style);
        let cx = x + w - cw;
        self.screen.set_str(
            (cx, y),
            &self.clip(&count, cw).0,
            surface.fg(base_fg(&self.theme.header)),
        );
    }

    /// Modal diffstat: file names with a scaled, colored +/- bar, like
    /// `git diff --stat`, floating over the diff on a secondary-accent surface.
    fn render_stat_modal(&mut self) {
        let w = self.screen.width();
        let h = self.screen.height();
        let (nf, add, del) = diff::totals(&self.files);
        let on_bg = self.theme.dialog.clone();

        if self.files.is_empty() {
            self.modal_hit = None;
            let (ix, iy) = self.draw_box(24, 1);
            self.screen.set_str((ix, iy), "no changes", on_bg.clone());
            return;
        }

        let name_w = self
            .files
            .iter()
            .map(|f| self.width(f.path()) as usize)
            .max()
            .unwrap_or(10)
            .clamp(10, (w as usize).saturating_sub(24));
        let count_w = 5usize;
        let bar_w = 24usize.min((w as usize).saturating_sub(name_w + count_w + 8));
        // The summary line can be wider than the file rows; size the box to the
        // larger of the two so it never gets clipped.
        let summary = format!("{nf} files changed, {add} insertion(s)(+), {del} deletion(s)(-)");
        let list_w = name_w + count_w + bar_w + 5;
        let inner_w = list_w
            .max(self.width(&summary) as usize)
            .min((w as usize).saturating_sub(2)) as u16;
        // Rows: one per file (capped to fit) + a blank + summary line.
        let max_rows = (h.saturating_sub(6)) as usize;
        let list_h = self.files.len().min(max_rows.max(1));
        let inner_h = (list_h + 2) as u16;
        let (ix, iy) = self.draw_box(inner_w, inner_h);

        // Scroll the list so the selected file stays visible.
        let start = self.selected.saturating_sub(list_h.saturating_sub(1));
        // Outer box spans one border cell around the inner (ix, iy) region.
        self.modal_hit = Some((
            ix - 1,
            iy - 1,
            ix + inner_w + 1,
            iy + inner_h + 1,
            iy,
            list_h,
            start,
        ));
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
            let marker = if selected { "▸ " } else { "  " };
            let name = self.shorten(file.path(), name_w as u16);
            let name_style = if selected {
                on_bg.clone().bold()
            } else {
                on_bg.clone()
            };
            let pad = (name_w as u16).saturating_sub(self.width(&name)) as usize;
            self.screen
                .set_str((ix, y), &format!("{marker}{name}{}", " ".repeat(pad)), name_style);
            let scaled = |n: usize| if n == 0 { 0 } else { ((n * bar_w) / max_count).max(1) };
            let ap = scaled(a);
            let dp = scaled(d).min(bar_w.saturating_sub(ap));
            // Right cluster: the count column then the +/- bar, both flush to the
            // box's right edge. Names stay left; the gap floats in the middle.
            let count = format!("{:>count_w$}", a + d);
            let cx = ix + inner_w - bar_w as u16 - 1 - count_w as u16;
            self.screen.set_str((cx, y), &count, on_bg.clone());
            let bstart = ix + inner_w - (ap + dp) as u16;
            self.screen
                .set_str((bstart, y), &"+".repeat(ap), on_bg.clone().fg(base_fg(&self.theme.add)));
            self.screen.set_str(
                (bstart + ap as u16, y),
                &"-".repeat(dp),
                on_bg.clone().fg(base_fg(&self.theme.remove)),
            );
        }

        // Summary line (already sized into inner_w above).
        self.screen.set_str(
            (ix, iy + inner_h - 1),
            &self.clip(&summary, inner_w).0,
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
                    RowKind::Hunk => self.theme.header_bg,
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
        let left_w = self.split_left_w(width);
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
                RowKind::Hunk => {
                    let hbg = cbg.or(self.theme.header_bg);
                    self.draw_diff_row(r, x, width, y, hbg, Gut::Both);
                    continue;
                }
                RowKind::Note => {
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
            let mut dv = self.theme.sidebar_border.clone();
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
        let num_w: u16 = self.gutter_w(gut);
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
                let (s, _) = self.clip(&r.spans[0].text, width);
                self.screen
                    .set_str((cx, y), &s, bg(self.theme.header.clone()));
                return;
            }
            RowKind::Note => {
                let (s, _) = self.clip(&r.spans[0].text, width);
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
            let (text, cw) = self.clip(&span.text, remaining as u16);
            if text.is_empty() {
                continue;
            }
            let mut style = if self.config.syntax_enabled() {
                Style::default().fg(span.fg.or_else(|| base_fg(base)))
            } else {
                base.clone()
            };
            if span.changed {
                if let Some(bgc) = emph_bg {
                    style = style.bg(bgc).bold();
                }
            }
            self.screen.set_str((cx, y), &text, bg(style));
            cx += cw;
        }
    }

    pub fn finish(self) -> io::Result<()> {
        self.screen.finish()
    }

    /// Truncate `s` to at most `width` display columns, respecting grapheme
    /// clusters and wide characters. Uses the screen's own width mode + EAW
    /// policy so a cell budget matches what `set_str` actually paints.
    fn clip(&self, s: &str, width: u16) -> (String, u16) {
        fit(self.screen.grapheme_cells(s), width)
    }

    /// Display width of `s` in terminal columns under the screen's width mode.
    fn width(&self, s: &str) -> u16 {
        self.screen.str_width(s)
    }

    /// Shorten `s` to at most `width` display columns, keeping the tail (e.g. a
    /// filename) visible behind a leading ellipsis. Width-aware so wide/non-ASCII
    /// paths never overflow or split a cluster.
    fn shorten(&self, s: &str, width: u16) -> String {
        if self.width(s) <= width {
            return s.to_string();
        }
        let ell = self.screen.grapheme_width("…") as u16;
        let cells: Vec<(&str, u8)> = self.screen.grapheme_cells(s).collect();
        format!("…{}", fit_tail(&cells, width.saturating_sub(ell)))
    }
}

/// Replace tabs with spaces to the next tab stop, tracking column across spans
/// so indentation lines up. Terminals give `\t` zero width in the grapheme
/// model, so unexpanded tabs would collapse to nothing on screen.
// ponytail: tab stops count codepoints, not display width; a tab after a wide
// char lands one column early. Switch to grapheme width here if that matters.
fn expand_tabs(spans: &mut [Span], tab: usize) {
    if tab == 0 {
        return;
    }
    let mut col = 0usize;
    for span in spans.iter_mut() {
        if !span.text.contains('\t') {
            col += span.text.chars().count();
            continue;
        }
        let mut out = String::with_capacity(span.text.len());
        for ch in span.text.chars() {
            if ch == '\t' {
                let n = tab - (col % tab);
                out.extend(std::iter::repeat(' ').take(n));
                col += n;
            } else {
                out.push(ch);
                col += 1;
            }
        }
        span.text = out;
    }
}

fn base_fg(style: &Style) -> Option<Color> {
    style.fg
}

/// Flatten a single file diff into styled display rows. Free-standing so it can
/// run on the startup prefetch thread as well as the lazy main-thread path.
fn build_file_rows(file: &FileDiff, hl: &Highlighter, intraline: bool, tab: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    if file.is_binary {
        rows.push(Row::new(
            RowKind::Note,
            None,
            None,
            vec![Span {
                fg: None,
                changed: false,
                text: "Binary file — no textual diff".into(),
            }],
        ));
        return rows;
    }

    for hunk in &file.hunks {
        rows.push(Row::new(
            RowKind::Hunk,
            None,
            None,
            vec![Span {
                fg: None,
                changed: false,
                text: hunk.header.clone(),
            }],
        ));
        let mut fh = hl.file(file.path());
        for line in &hunk.lines {
            let syntax_spans = fh.line(&line.content);
            let mut spans = merge(syntax_spans, &line.segments, intraline);
            expand_tabs(&mut spans, tab);
            let kind = match line.kind {
                LineKind::Add => RowKind::Add,
                LineKind::Remove => RowKind::Remove,
                LineKind::Context => RowKind::Context,
            };
            rows.push(Row::new(kind, line.old_no, line.new_no, spans));
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

/// Fit `(cluster, width)` pairs into at most `width` columns without splitting a
/// wide cluster. Returns the fitted string and the columns it occupies.
fn fit<'a>(cells: impl Iterator<Item = (&'a str, u8)>, width: u16) -> (String, u16) {
    let mut out = String::new();
    let mut w = 0u16;
    for (g, gw) in cells {
        let gw = gw as u16;
        if w + gw > width {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    (out, w)
}

/// Take clusters from the end of `cells` that fit in `budget` columns, keeping
/// the tail intact without splitting a wide cluster. Used by `shorten`.
fn fit_tail(cells: &[(&str, u8)], budget: u16) -> String {
    let mut w = 0u16;
    let mut start = cells.len();
    for i in (0..cells.len()).rev() {
        let cw = cells[i].1 as u16;
        if w + cw > budget {
            break;
        }
        w += cw;
        start = i;
    }
    cells[start..].iter().map(|(g, _)| *g).collect()
}

#[cfg(test)]
mod tests {
    use super::{expand_tabs, fit, fit_tail, slice_cells, text_cells, Sel, Span};
    use uncurses::text::{grapheme_cells, WidthMode};

    // Exercise the same fitting logic clip() uses; clip() itself needs a live
    // screen, so we feed grapheme_cells directly here.
    fn clip(s: &str, width: u16) -> (String, u16) {
        fit(grapheme_cells(s, WidthMode::Grapheme, false), width)
    }

    fn sel(a_row: usize, a_col: usize, c_row: usize, c_col: usize) -> Sel {
        Sel { a_row, a_col, c_row, c_col, dragging: false, pane: None }
    }

    #[test]
    fn split_selection_is_confined_to_its_pane() {
        use super::{App, Pane, RowKind};
        // Left pane holds context, removals, and headers, not additions.
        assert!(App::row_in_pane(RowKind::Remove, Some(Pane::Left)));
        assert!(App::row_in_pane(RowKind::Context, Some(Pane::Left)));
        assert!(App::row_in_pane(RowKind::Hunk, Some(Pane::Left)));
        assert!(!App::row_in_pane(RowKind::Add, Some(Pane::Left)));
        // Right pane holds context, additions, and headers, not removals.
        assert!(App::row_in_pane(RowKind::Add, Some(Pane::Right)));
        assert!(App::row_in_pane(RowKind::Context, Some(Pane::Right)));
        assert!(!App::row_in_pane(RowKind::Remove, Some(Pane::Right)));
        // Unified view (None) selects every row kind.
        assert!(App::row_in_pane(RowKind::Add, None));
        assert!(App::row_in_pane(RowKind::Remove, None));
    }

    fn span(text: &str) -> Span {
        Span { fg: None, changed: false, text: text.into() }
    }

    #[test]
    fn expand_tabs_aligns_to_stops_across_spans() {
        // Leading tab expands to a full stop; a tab mid-column fills to the
        // next multiple of the tab width, counting columns across span breaks.
        let mut spans = vec![span("\tif"), span(" x\t{")];
        expand_tabs(&mut spans, 4);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "    if x    {");
        // tab=0 disables expansion (tabs left untouched).
        let mut spans = vec![span("\tx")];
        expand_tabs(&mut spans, 0);
        assert_eq!(spans[0].text, "\tx");
    }

    #[test]
    fn selection_orders_reading_wise() {
        // Dragging up/backward still yields start-before-end.
        let s = sel(5, 8, 1, 2);
        assert_eq!(s.ordered(), (1, 2, 5, 8));
        let s = sel(1, 2, 5, 8);
        assert_eq!(s.ordered(), (1, 2, 5, 8));
    }

    #[test]
    fn empty_selection_detected() {
        assert!(sel(3, 4, 3, 4).is_empty());
        assert!(!sel(3, 4, 3, 5).is_empty());
    }

    #[test]
    fn slice_cells_clamps_and_trims() {
        // Whole line via an oversized end index, trailing blanks trimmed.
        let c = text_cells("    return 1   ");
        assert_eq!(slice_cells(&c, 0, usize::MAX), "    return 1");
        // A mid-line span.
        let c = text_cells("def foo():");
        assert_eq!(slice_cells(&c, 4, 7), "foo");
        // Start past the end yields empty.
        let c = text_cells("abc");
        assert_eq!(slice_cells(&c, 9, 9), "");
    }

    #[test]
    fn wide_chars_occupy_two_cells() {
        // A wide grapheme takes two cells (glyph + continuation), so a cell
        // index maps 1:1 to a screen column. The continuation contributes "".
        let c = text_cells("a世b");
        assert_eq!(c.len(), 4);
        assert_eq!(slice_cells(&c, 0, 4), "a世b");
        // Selecting only the wide cell (not its continuation) still yields it.
        assert_eq!(slice_cells(&c, 1, 2), "世");
    }

    #[test]
    fn clip_counts_display_columns() {
        // ASCII: one column per char.
        assert_eq!(clip("hello", 3), ("hel".into(), 3));
        assert_eq!(clip("hi", 10), ("hi".into(), 2));
        // Wide chars cost two columns; a budget of 3 fits one wide + nothing
        // more (the next wide would overflow), not two chars.
        assert_eq!(clip("世界", 3), ("世".into(), 2));
        assert_eq!(clip("世界", 4), ("世界".into(), 4));
        // A wide char never gets split across the boundary.
        assert_eq!(clip("a世", 2), ("a".into(), 1));
    }

    #[test]
    fn fit_tail_keeps_end_width_aware() {
        let cells: Vec<_> = grapheme_cells("src/世界.rs", WidthMode::Grapheme, false).collect();
        // Budget keeps the tail; a wide cluster is never split at the boundary.
        assert_eq!(fit_tail(&cells, 5), "界.rs");
        assert_eq!(fit_tail(&cells, 6), "界.rs");
        assert_eq!(fit_tail(&cells, 7), "世界.rs");
        // Zero budget keeps nothing.
        assert_eq!(fit_tail(&cells, 0), "");
    }
}
