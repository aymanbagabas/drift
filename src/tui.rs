//! The terminal UI: a scrollable, syntax-highlighted diff pane with a single
//! bottom footer, driven by an uncurses `Screen`.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

use uncurses::buffer::{Bounded, Line, SurfaceMut};
use uncurses::cell::Cell;
use uncurses::color::Color;
use uncurses::event::{Event, Key, KeyModifiers, MouseButton};
use uncurses::screen::{MouseTracking, Screen, ScreenOptions};
use uncurses::style::{AttrFlags, Style};
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
    statusbar_search: Style,
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
    // Search match highlight (all hits) and the current hit, reusing the
    // accent palette so every theme gets it without per-theme tuning.
    search_match: Style,
    search_current: Style,
    /// Idle-mascot colours: filled body and antenna/reaction accent. The face
    /// glyphs (eyes) are always black, drawn on top of the body.
    mascot_body: Color,
    mascot_accent: Color,
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
            statusbar_search: sty("statusbar-search", "secondary surface"),
            statusbar_watch: sty("statusbar-watch", "background add bold"),
            statusbar_help: sty("statusbar-help", "background secondary bold"),
            help_key: sty("help-key", "muted bold"),
            help_desc: sty("help-desc", "muted faint"),
            dialog: sty("dialog", "foreground background"),
            dialog_border: sty("dialog-border", "surface"),
            sidebar_border: sty("sidebar-border", "surface"),
            search_match: sty("search-match", "background secondary bold"),
            search_current: sty("search-current", "background primary bold"),
            mascot_body: pal.color("primary").unwrap_or(Color::Indexed(99)),
            mascot_accent: pal.color("secondary").unwrap_or(Color::Indexed(75)),
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
    File,
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

/// Mascot bounding box in cells: an 11-wide Space-Invaders–style alien monster
/// (like 👾) drawn purely with half-blocks (two vertical pixels per cell) —
/// horns, a wide head with two eyes, side arms, and legs. Its two arcade walk
/// frames alternate for an idle wiggle. Height is fixed so movement and poke
/// hit-testing stay stable.
const MASCOT_W: u16 = 11;
const MASCOT_H: u16 = 4;

/// The mascot's current expression, drawn as eye pixels (no letters, no mouth —
/// like the 👾 emoji). Idle moods (Normal/Happy/Wink/Look/Squint/Cool/Curious)
/// are rolled at random and held briefly; Blink flickers over any of them;
/// poke/drag force Surprised and Dizzy.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Face {
    Normal,
    Blink,
    Happy,
    Wink,
    Look,
    Squint,
    Cool,
    Curious,
    Surprised,
    Dizzy,
}

impl Face {
    /// The idle moods that get rolled at random (never the event-driven ones).
    /// Weighted by repetition so Normal shows most often.
    const IDLE: [Face; 10] = [
        Face::Normal,
        Face::Normal,
        Face::Normal,
        Face::Happy,
        Face::Happy,
        Face::Wink,
        Face::Look,
        Face::Squint,
        Face::Cool,
        Face::Curious,
    ];
}

/// The idle-screen mascot: a little antenna'd bot that drifts around the empty
/// diff body, breathes (its belly puffs in and out), and makes a surprised face
/// for a moment when poked. Purely decorative — it exists only while there are
/// no changes to show.
struct Mascot {
    /// Top-left of the sprite, in body cells (fractional for smooth drift).
    x: f32,
    y: f32,
    /// Drift velocity in cells/second, re-rolled at every `next_turn`.
    vx: f32,
    vy: f32,
    born: Instant,
    last: Instant,
    next_turn: Instant,
    poke_until: Option<Instant>,
    /// True while the user is dragging it: drift pauses and it follows the
    /// pointer instead.
    dragging: bool,
    /// Recent horizontal drag direction, negated so the limbs trail *opposite*
    /// the drag; decays back to 0 once you let go.
    lean: f32,
    /// xorshift64 state (seeded from the wall clock) for the random walk.
    rng: u64,
    /// Current idle expression and when to roll the next one; `mood_side` picks
    /// the direction for sided faces (wink/glance).
    mood: Face,
    mood_until: Instant,
    mood_side: i16,
}

impl Mascot {
    fn new(bw: u16, bh: u16) -> Self {
        let now = Instant::now();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
            | 1;
        let mut m = Mascot {
            x: (bw.saturating_sub(MASCOT_W) / 2) as f32,
            y: (bh.saturating_sub(MASCOT_H) / 2) as f32,
            vx: 0.0,
            vy: 0.0,
            born: now,
            last: now,
            next_turn: now,
            poke_until: None,
            dragging: false,
            lean: 0.0,
            rng: seed,
            mood: Face::Normal,
            mood_until: now,
            mood_side: 1,
        };
        m.turn();
        m.roll_mood();
        m
    }

    /// xorshift64 mapped to `[0, 1)`.
    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        ((x >> 40) as f32) / ((1u32 << 24) as f32)
    }

    /// Roll a fresh drift and schedule the next course change (0.6–1.8s out).
    /// Uses a random *heading* with a guaranteed minimum speed so it never rolls
    /// a near-zero velocity and appears to freeze mid-screen.
    fn turn(&mut self) {
        let (a, b, c) = (self.rand(), self.rand(), self.rand());
        let angle = a * std::f32::consts::TAU;
        let speed = 3.0 + b * 4.0; // ~[3, 7] cells/s — always visibly moving
        self.vx = angle.cos() * speed;
        self.vy = angle.sin() * speed * 0.6; // flatten vertical drift a touch
        self.next_turn = Instant::now() + Duration::from_millis(600 + (c * 1200.0) as u64);
    }

    /// Pick the next idle expression at random and hold it ~1.2–3s. Sided faces
    /// (wink/glance) also get a random left/right bias.
    fn roll_mood(&mut self) {
        let (a, b, c) = (self.rand(), self.rand(), self.rand());
        self.mood = Face::IDLE[(a * Face::IDLE.len() as f32) as usize % Face::IDLE.len()];
        self.mood_side = if b < 0.5 { -1 } else { 1 };
        self.mood_until = Instant::now() + Duration::from_millis(1200 + (c * 1800.0) as u64);
    }

    /// Advance the drift within a `bw`×`bh` body, bouncing off the edges. While
    /// being dragged it only settles its limbs; the pointer sets its position.
    fn tick(&mut self, bw: u16, bh: u16) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.1);
        self.last = now;
        self.lean *= 0.75_f32.powf(dt / 0.05); // ease limbs back to rest
        if self.dragging {
            return; // held: follows the pointer, so no drift
        }
        if now >= self.next_turn {
            self.turn();
        }
        if now >= self.mood_until {
            self.roll_mood();
        }
        self.x += self.vx * dt;
        self.y += self.vy * dt;
        let (maxx, maxy) = (
            bw.saturating_sub(MASCOT_W) as f32,
            bh.saturating_sub(MASCOT_H) as f32,
        );
        if self.x < 0.0 {
            self.x = 0.0;
            self.vx = -self.vx;
        } else if self.x > maxx {
            self.x = maxx;
            self.vx = -self.vx;
        }
        if self.y < 0.0 {
            self.y = 0.0;
            self.vy = -self.vy;
        } else if self.y > maxy {
            self.y = maxy;
            self.vy = -self.vy;
        }
    }

    /// Bounding box in body cells: (col0, row0, col1, row1), ends exclusive.
    fn bbox(&self) -> (u16, u16, u16, u16) {
        let (x, y) = (self.x.round() as u16, self.y.round() as u16);
        (x, y, x + MASCOT_W, y + MASCOT_H)
    }

    /// React to a poke: surprised face for a moment, then dart off.
    fn poke(&mut self) {
        self.poke_until = Some(Instant::now() + Duration::from_millis(700));
        self.turn();
        self.vx *= 1.8;
        self.vy *= 1.8;
    }

    fn grab(&mut self) {
        self.dragging = true;
    }

    /// Let go: resume drifting from a fresh random heading.
    fn release(&mut self) {
        self.dragging = false;
        self.turn();
    }

    /// Move to a dragged top-left `(tx, ty)` (body cells), clamped in-bounds,
    /// leaning the limbs opposite the horizontal motion.
    fn drag_to(&mut self, tx: f32, ty: f32, bw: u16, bh: u16) {
        self.lean = -(tx - self.x).clamp(-1.5, 1.5);
        let (maxx, maxy) = (
            bw.saturating_sub(MASCOT_W) as f32,
            bh.saturating_sub(MASCOT_H) as f32,
        );
        self.x = tx.clamp(0.0, maxx);
        self.y = ty.clamp(0.0, maxy);
    }

    /// How far to shift the antenna/limb rows (in cells) relative to the body,
    /// so they trail opposite the drag. −1, 0, or +1.
    fn lean_offset(&self) -> i16 {
        if self.lean > 0.4 {
            1
        } else if self.lean < -0.4 {
            -1
        } else {
            0
        }
    }

    /// Which of the two arcade walk frames to show right now; alternating them
    /// makes the alien wiggle in place. Toggles a bit under a second.
    fn stride(&self) -> bool {
        (self.born.elapsed().as_secs_f32() / 0.7) as u64 % 2 == 0
    }

    /// Whether a poke flash is active (used to pop the body colour).
    fn flashing(&self) -> bool {
        self.poke_until.is_some_and(|u| Instant::now() < u)
    }

    /// Which way the eyes glance while drifting, so it looks where it's going.
    fn gaze(&self) -> i16 {
        if self.vx > 1.0 {
            1
        } else if self.vx < -1.0 {
            -1
        } else {
            0
        }
    }

    /// The current expression. Dragging looks dizzy, a poke is surprised, and
    /// otherwise it wears its current idle mood with an occasional blink
    /// flickering over the top.
    fn face(&self) -> Face {
        if self.dragging {
            return Face::Dizzy;
        }
        if self.flashing() {
            return Face::Surprised;
        }
        let t = self.born.elapsed().as_millis() as u64;
        if t % 4200 < 160 {
            return Face::Blink;
        }
        self.mood
    }

    /// Stamp one face pixel (face/accent material) at grid `(r, c)`, ignoring
    /// out-of-range coordinates.
    fn stamp(grid: &mut [[u8; MASCOT_W as usize]], r: usize, c: i16, m: u8) {
        if r < grid.len() && (0..MASCOT_W as i16).contains(&c) {
            grid[r][c as usize] = m;
        }
    }

    /// The mascot as a material bitmap, two vertical pixels per rendered cell.
    /// Bytes: `.` transparent, `B` body, `A` accent (dizzy eyes), `D` eyes. It's
    /// the Space-Invaders alien: two authentic arcade walk frames alternate for
    /// an idle wiggle, the horns/legs sway opposite a drag, and — like the 👾
    /// emoji — the expression lives entirely in the eyes (no mouth). Rendered
    /// with half-blocks.
    fn frame(&self) -> Vec<[u8; MASCOT_W as usize]> {
        const W: usize = MASCOT_W as usize;
        // The two canonical Space-Invaders "crab" walk frames, as solid bodies.
        let frame_a: [&[u8; W]; 8] = [
            b"..B.....B..",
            b"B..B...B..B",
            b"B.BBBBBBB.B",
            b"BBBBBBBBBBB",
            b"BBBBBBBBBBB",
            b".BBBBBBBBB.",
            b"..B.....B..",
            b".B.......B.",
        ];
        let frame_b: [&[u8; W]; 8] = [
            b"..B.....B..",
            b"...B...B...",
            b"..BBBBBBB..",
            b".BBBBBBBBB.",
            b"BBBBBBBBBBB",
            b"B.BBBBBBB.B",
            b"B.B.....B.B",
            b"...BB.BB...",
        ];
        // The two arcade walk frames alternate to give an idle wiggle.
        let src = if self.stride() { frame_a } else { frame_b };

        // Rows 0-1 (horns) and 6-7 (legs/feet) are the limbs: they slide
        // opposite the drag; the core body stays put.
        let lean = self.lean_offset();
        let mut g: Vec<[u8; W]> = Vec::with_capacity(8);
        for (r, row) in src.iter().enumerate() {
            let limb = matches!(r, 0 | 1 | 6 | 7);
            let mut out = [b'.'; W];
            for (c, &b) in row.iter().enumerate() {
                if b == b'B' {
                    let dc = if limb { c as i16 + lean } else { c as i16 };
                    if (0..W as i16).contains(&dc) {
                        out[dc as usize] = b'B';
                    }
                }
            }
            g.push(out);
        }

        // Eyes at columns 3 and 7; expression is all in the eyes.
        let (lc, rc) = (3i16, 7i16);
        match self.face() {
            Face::Blink => {} // shut: the body closes over them
            Face::Surprised => {
                for c in [lc, rc] {
                    Self::stamp(&mut g, 3, c, b'D');
                    Self::stamp(&mut g, 4, c, b'D'); // wide-open
                }
            }
            Face::Dizzy => {
                for c in [lc, rc] {
                    Self::stamp(&mut g, 3, c, b'A');
                    Self::stamp(&mut g, 4, c, b'A'); // accent-coloured daze
                }
            }
            Face::Happy => {
                for c in [lc, rc] {
                    Self::stamp(&mut g, 2, c, b'D'); // raised, cheerful
                }
            }
            Face::Wink => {
                // One eye open, the other shut (side chosen by mood_side).
                let (open, _shut) = if self.mood_side < 0 { (rc, lc) } else { (lc, rc) };
                Self::stamp(&mut g, 3, open, b'D');
            }
            Face::Look => {
                // A steady sidelong glance, both eyes shifted together.
                let z = self.mood_side;
                Self::stamp(&mut g, 3, lc + z, b'D');
                Self::stamp(&mut g, 3, rc + z, b'D');
            }
            Face::Squint => {
                // Eyes drawn inward for a suspicious, narrowed look.
                Self::stamp(&mut g, 3, lc + 1, b'D');
                Self::stamp(&mut g, 3, rc - 1, b'D');
            }
            Face::Cool => {
                // A horizontal visor bar — shades on.
                for c in lc..=rc {
                    Self::stamp(&mut g, 3, c, b'D');
                }
            }
            Face::Curious => {
                // Wide, high, spread-apart eyes: intrigued.
                Self::stamp(&mut g, 2, lc - 1, b'D');
                Self::stamp(&mut g, 2, rc + 1, b'D');
            }
            Face::Normal => {
                let z = self.gaze();
                Self::stamp(&mut g, 3, lc + z, b'D');
                Self::stamp(&mut g, 3, rc + z, b'D');
            }
        }
        g
    }
}


/// The printable text a key produces (glyph, so uppercase and shifted symbols
/// come through), or None for control/named keys and modifier chords, for
/// feeding the search prompt.
fn typed_text(k: &Key) -> Option<String> {
    if k.modifiers.intersects(KeyModifiers::CTRL | KeyModifiers::ALT | KeyModifiers::SUPER) {
        return None;
    }
    if let Some(t) = &k.text {
        if !t.is_empty() && !t.chars().any(|c| c.is_control()) {
            return Some(t.clone());
        }
    }
    match k.char() {
        Some(c) if !c.is_control() => Some(c.to_string()),
        _ => None,
    }
}

/// Find every regex match in a row's display cells, returning (start_col,
/// end_col) column ranges. Continuation cells (wide-char tails) carry no text;
/// a hit's end column extends over the last matched grapheme's full width.
/// Zero-width matches are skipped.
fn match_cells(cells: &[Cell], re: &Regex) -> Vec<(usize, usize)> {
    let mut hay = String::new();
    let mut byte_col: Vec<usize> = Vec::new();
    for (col, cell) in cells.iter().enumerate() {
        if cell.is_continuation() {
            continue;
        }
        let t = cell.content();
        for _ in t.bytes() {
            byte_col.push(col);
        }
        hay.push_str(t);
    }
    let mut out = Vec::new();
    for m in re.find_iter(&hay) {
        if m.start() == m.end() {
            continue;
        }
        let b0 = m.start();
        let b1 = m.end();
        let start = byte_col[b0];
        let last = byte_col[b1 - 1];
        let end = last + cells[last].width().max(1) as usize;
        out.push((start, end));
    }
    out
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
    /// The whole diff as one continuous document: every file's rows
    /// concatenated, each preceded by a `RowKind::File` header. `file_starts[i]`
    /// is the doc-row index of file `i`'s header, so the file under the cursor
    /// (`selected`) and the file picker's scroll target are both derived from a
    /// row index. Built lazily by the prefetch worker after startup.
    doc_rows: Vec<Row>,
    file_starts: Vec<usize>,
    /// Background row builder feeding `doc_rows` after startup so the first
    /// frame isn't blocked on syntax highlighting.
    prefetch: Option<Receiver<(usize, Vec<Row>)>>,
    /// In-flight async worktree poll: a background thread running the diff so
    /// the main loop never blocks on git, even on huge repos. `None` when idle.
    poll_worker: Option<Receiver<String>>,
    /// In-flight async document rebuild (reload/poll/expand): a background
    /// thread parses the new diff and builds the whole document off the main
    /// thread. The old document stays on screen and interactive until the new
    /// one arrives, then swaps in atomically with the cursor position restored.
    /// `None` when idle; superseded rebuilds are dropped (their sends fail).
    rebuild_worker: Option<Receiver<Rebuilt>>,
    /// Search state (whole-document). `query` is the last confirmed pattern ("" =
    /// no active search); `input` is Some while typing in the `/` prompt.
    /// `matches` holds (doc_row, cell_start, cell_end) hits across all files;
    /// `match_i` is the current hit for `n`/`N` navigation and highlighting.
    query: String,
    input: Option<String>,
    /// (cursor, scroll, query) snapshot taken when the `/` prompt opens, so Esc
    /// restores the pre-search view (Neovim incsearch behaviour).
    search_return: Option<(usize, usize, String)>,
    matches: Vec<(usize, usize, usize)>,
    match_i: Option<usize>,
    selected: usize,
    /// Top visible row (viewport offset).
    scroll: usize,
    /// Selected/highlighted row within the diff (tig-style cursor line).
    cursor: usize,
    /// Horizontal scroll offset in display columns; the gutter and +/- sign
    /// stay pinned while the line content shifts left by this many columns.
    hscroll: usize,
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
    /// Idle-screen mascot, lazily created while there are no changes to show
    /// and cleared as soon as a diff appears.
    mascot: Option<Mascot>,
    /// Grab offset into the mascot's bounding box while it's being dragged.
    mascot_grab: Option<(u16, u16)>,
    /// Force the mascot to appear over the diff, toggled by tapping Esc thrice.
    mascot_pinned: bool,
    /// Timestamps of recent Esc taps, for detecting the triple-tap toggle.
    esc_taps: (u8, Option<Instant>),
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
            doc_rows: Vec::new(),
            file_starts: Vec::new(),
            prefetch: None,
            poll_worker: None,
            rebuild_worker: None,
            query: String::new(),
            input: None,
            search_return: None,
            matches: Vec::new(),
            match_i: None,
            selected: 0,
            scroll: 0,
            cursor: 0,
            hscroll: 0,
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
            mascot: None,
            mascot_grab: None,
            mascot_pinned: false,
            esc_taps: (0, None),
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
        self.doc_rows.clear();
        self.file_starts.clear();
        self.spawn_prefetch();
    }

    /// Spawn a worker that builds every file's body rows in order and streams
    /// them back, so the continuous document fills in from the top without
    /// blocking the first frame on syntax highlighting.
    fn spawn_prefetch(&mut self) {
        if self.files.is_empty() {
            self.prefetch = None;
            return;
        }
        let files = Arc::new(self.files.clone());
        let hl = Arc::clone(&self.highlighter);
        let intraline = self.config.intraline_enabled();
        let tab = self.config.tab_width;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for idx in 0..files.len() {
                let rows = build_file_rows(&files[idx], &hl, intraline, tab);
                if tx.send((idx, rows)).is_err() {
                    return; // receiver dropped (reload/quit): stop early.
                }
            }
        });
        self.prefetch = Some(rx);
    }

    /// Append any file bodies the prefetch worker has finished to the document.
    /// The worker sends files in order, so each arrival appends below the
    /// existing rows (headers + bodies stay in file order).
    fn drain_prefetch(&mut self) {
        let Some(rx) = self.prefetch.take() else {
            return;
        };
        let mut got = false;
        loop {
            match rx.try_recv() {
                Ok((idx, body)) => {
                    if let Some(f) = self.files.get(idx) {
                        self.file_starts.push(self.doc_rows.len());
                        self.doc_rows.push(file_header_row(f));
                        self.doc_rows.extend(body);
                        got = true;
                    }
                }
                Err(TryRecvError::Empty) => {
                    self.prefetch = Some(rx);
                    break;
                }
                Err(TryRecvError::Disconnected) => break, // worker done: drop rx.
            }
        }
        if got {
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
        // A piped diff (pager mode) is one-shot: stdin is already consumed, so
        // re-reading yields nothing and would wipe the view. Nothing to reload.
        if matches!(self.source, Source::Stdin) {
            return;
        }
        let text = self.source.diff(&self.effective_opts()).unwrap_or_default();
        self.rebuild_from(text);
    }

    /// Kick off an async rebuild from already-fetched diff text. Shared by
    /// `reload` and the worktree poll. The parse + whole-document build (syntax
    /// highlighting included) runs on a background thread; the current document
    /// stays on screen and fully interactive until the new one is ready, then
    /// `drain_rebuild` swaps it in with the cursor position restored. A newer
    /// rebuild simply replaces the receiver, so the stale worker's send fails
    /// and it exits. `last_diff` is set now so the poll won't re-fire the same
    /// text while the build is in flight.
    fn rebuild_from(&mut self, text: String) {
        // Drop any in-flight startup prefetch so its (now stale) rows can't
        // land after the swap.
        self.prefetch = None;
        self.last_diff = text.clone();
        let hl = Arc::clone(&self.highlighter);
        let intraline = self.config.intraline_enabled();
        let tab = self.config.tab_width;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let files = diff::parse(&text);
            let raw = diff::split_files(&text);
            let (doc, starts) = assemble_document(&files, &hl, intraline, tab);
            let _ = tx.send(Rebuilt {
                files,
                raw,
                doc,
                starts,
            });
        });
        self.rebuild_worker = Some(rx);
    }

    /// Swap in a finished background rebuild, restoring the cursor to the same
    /// place in the new document. The anchor is captured against the *current*
    /// (old) document at swap time, so navigation during the build is honoured.
    fn drain_rebuild(&mut self) {
        let Some(rx) = self.rebuild_worker.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(r) => {
                let anchor = self.capture_anchor();
                self.files = r.files;
                self.raw_files = r.raw;
                self.doc_rows = r.doc;
                self.file_starts = r.starts;
                let row = self.resolve_anchor(&anchor);
                self.cursor = row;
                self.scroll = row.saturating_sub(anchor.screen_off);
                self.move_cursor(0);
                self.refresh_search();
                // A wholesale document swap can move content past rows that were
                // blank before; force a full repaint so nothing stale lingers.
                self.screen.invalidate();
            }
            Err(TryRecvError::Empty) => self.rebuild_worker = Some(rx), // still building
            Err(TryRecvError::Disconnected) => {} // superseded/errored: drop rx
        }
    }

    /// Capture where the cursor sits, semantically, so it can be found again in
    /// a rebuilt document: which file (by path), which line (or hunk), and how
    /// far down the screen it was.
    fn capture_anchor(&self) -> Anchor {
        let screen_off = self.cursor.saturating_sub(self.scroll);
        let file = self.file_at(self.cursor);
        let path = self
            .files
            .get(file)
            .map(|f| f.path().to_string())
            .unwrap_or_default();
        let target = match self.doc_rows.get(self.cursor).map(|r| r.kind) {
            Some(RowKind::Add) | Some(RowKind::Remove) | Some(RowKind::Context) => {
                let r = &self.doc_rows[self.cursor];
                AnchorTarget::Line(r.old_no, r.new_no)
            }
            Some(RowKind::Hunk) => AnchorTarget::Hunk(self.hunk_ord_in_file(file, self.cursor)),
            _ => AnchorTarget::Start,
        };
        Anchor {
            path,
            target,
            screen_off,
        }
    }

    /// Resolve a captured anchor to a row in the current document. Falls back to
    /// the file's header (or the top, if the file is gone) when the exact line
    /// or hunk can't be found.
    fn resolve_anchor(&self, a: &Anchor) -> usize {
        resolve_anchor_row(&self.files, &self.file_starts, &self.doc_rows, a)
    }

    /// 0-based index of the hunk `cursor` sits on, counted within its own file.
    fn hunk_ord_in_file(&self, file: usize, cursor: usize) -> usize {
        let start = self.file_starts.get(file).copied().unwrap_or(0);
        self.doc_rows[start..=cursor]
            .iter()
            .filter(|r| r.kind == RowKind::Hunk)
            .count()
            .saturating_sub(1)
    }

    /// The file index whose rows contain document row `row`.
    fn file_at(&self, row: usize) -> usize {
        file_of_row(&self.file_starts, row)
    }

    /// Grow the folded context around the current hunk. The async rebuild's
    /// anchor pins the hunk the cursor is on back to the same screen row, so
    /// expanding doesn't scroll the view.
    fn expand_here(&mut self) {
        self.expand += 10;
        self.reload();
    }

    /// The whole continuous document (empty until the prefetch/build fills it).
    fn rows(&self) -> &[Row] {
        &self.doc_rows
    }

    /// Whether the cursor is currently on a hunk header row.
    fn on_hunk(&self) -> bool {
        self.rows()
            .get(self.cursor)
            .is_some_and(|r| r.kind == RowKind::Hunk)
    }

    /// Content rows that fit below the sticky-header band: the physical body
    /// height minus however many headers are currently pinned at the top.
    fn viewport_rows(&self) -> usize {
        self.body_h_screen().saturating_sub(self.sticky_h())
    }

    /// Physical rows of the body region (everything above the footer/help
    /// chrome), including the sticky-header band.
    fn body_h_screen(&self) -> usize {
        (self.screen.height() as usize).saturating_sub(self.chrome_h())
    }

    /// Document rows pinned at the top of the body for the current scroll: the
    /// enclosing file header and the current hunk header, but only once they've
    /// scrolled above the top visible content line (so a header still naturally
    /// on screen isn't duplicated). Capped to leave at least one content row.
    fn sticky_rows(&self) -> Vec<usize> {
        let rows = self.rows();
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        sticky_at(
            &kinds,
            &self.file_starts,
            self.scroll,
            self.body_h_screen(),
        )
    }

    fn sticky_h(&self) -> usize {
        self.sticky_rows().len()
    }

    /// Map a screen body-row `y` to the document row drawn there: the sticky
    /// band up top, then linear from `scroll` below it.
    fn screen_y_to_doc(&self, y: u16) -> usize {
        let sticky = self.sticky_rows();
        let k = sticky.len();
        let last = self.rows().len().saturating_sub(1);
        if (y as usize) < k {
            sticky[y as usize]
        } else {
            (self.scroll + (y as usize - k)).min(last)
        }
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

    /// The quick-help entries shown in the expandable footer grid. In pager
    /// mode (a static piped diff) the repo-driven affordances are inert, so
    /// they're dropped to keep the help honest.
    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        let piped = matches!(self.source, Source::Stdin);
        [
            ("j/k ↑/↓", "move"),
            ("h/l ←/→", "scroll x"),
            ("^d/^u", "half page"),
            ("g/G", "top/bottom"),
            ("{ }", "prev/next hunk"),
            ("[ ]", "prev/next file"),
            ("tab", "cycle files"),
            ("/", "search"),
            ("n/N", "next/prev match"),
            ("s", "split view"),
            ("f", "files"),
            ("b", "sidebar"),
            ("w", "watch on/off"),
            ("a", "untracked on/off"),
            ("enter", "expand context"),
            ("e", "edit in $EDITOR"),
            ("y", "copy line/selection"),
            ("Y", "copy file diff"),
            ("r", "refresh"),
            ("?", "toggle help"),
            ("q", "quit"),
        ]
        .into_iter()
        .filter(|(k, _)| !(piped && matches!(*k, "w" | "a" | "enter" | "r")))
        .collect()
    }

    /// Grid geometry for the help footer: (columns, rows, cell width). Packs
    /// entries into as many columns as fit the terminal width, charm-style.
    fn help_grid(&self) -> (usize, usize, usize) {
        let entries = self.help_entries();
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

    /// Scroll the viewport by `delta` rows and drag the cursor along only when
    /// an edge would push past it, so the cursor stays pinned in the document
    /// until it clips at the top/bottom of the window (mouse-wheel paging).
    fn scroll_page(&mut self, delta: isize) {
        if self.rows().is_empty() {
            return;
        }
        self.scroll_by(delta);
        let vh = self.viewport_rows();
        let last = self.rows().len() - 1;
        let bottom = (self.scroll + vh).saturating_sub(1).min(last);
        self.cursor = self.cursor.clamp(self.scroll, bottom);
        self.selected = self.file_at(self.cursor);
    }

    /// Columns available for line content: the terminal width minus the
    /// sidebar, the line-number gutter, and the +/- sign. In split view a line
    /// lives inside one half-width pane (with the narrower per-side gutter), so
    /// use the smaller pane's content width — that's what a line must scroll
    /// within, and it lets the widest line reveal fully in either pane.
    fn content_view_w(&self) -> u16 {
        let body = self.screen.width().saturating_sub(self.sidebar_w());
        if self.split {
            let left = self.split_left_w(body);
            let right = body.saturating_sub(left + 1);
            left.min(right)
                .saturating_sub(self.content_start(RowKind::Context, Gut::Old))
        } else {
            body.saturating_sub(self.content_start(RowKind::Context, Gut::Both))
        }
    }

    /// Furthest the content can scroll left: the widest content among the rows
    /// currently on screen, minus the visible content width. Recomputed per
    /// scroll tick (bounded by the viewport height, so cheap).
    fn max_hscroll(&self) -> usize {
        let vh = self.viewport_rows();
        let widest = self
            .rows()
            .iter()
            .skip(self.scroll)
            .take(vh)
            .map(|r| r.content.len())
            .max()
            .unwrap_or(0);
        widest.saturating_sub(self.content_view_w() as usize)
    }

    /// Scroll the line content horizontally by `delta` columns, clamped so it
    /// never scrolls past the widest visible line or before column 0.
    fn scroll_h(&mut self, delta: isize) {
        let max = self.max_hscroll() as isize;
        self.hscroll = (self.hscroll as isize + delta).clamp(0, max) as usize;
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
        let row = self.screen_y_to_doc(y);
        let (origin, cs) = self.pane_geom(rows[row].kind, pane);
        let len = rows[row].content.len();
        let col = (x.saturating_sub(origin + cs) as usize + self.hscroll).min(len);
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
            Some(_) if matches!(kind, RowKind::Hunk | RowKind::Note | RowKind::File) => {
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
                RowKind::Context | RowKind::Remove | RowKind::Hunk | RowKind::Note | RowKind::File
            ),
            Some(Pane::Right) => matches!(
                kind,
                RowKind::Context | RowKind::Add | RowKind::Hunk | RowKind::Note | RowKind::File
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
        // The "selected" file is simply whichever one the cursor sits in.
        self.selected = self.file_at(self.cursor);
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
        if self.file_starts.is_empty() {
            return;
        }
        let n = self.file_starts.len() as isize;
        let cur = self.file_at(self.cursor) as isize;
        let new = (cur + delta).clamp(0, n - 1) as usize;
        self.select_file_at(new);
    }

    /// Scroll the document to the start of file `idx`, pinning its header row to
    /// the top of the viewport.
    fn select_file_at(&mut self, idx: usize) {
        if let Some(&start) = self.file_starts.get(idx) {
            self.cursor = start;
            self.scroll = start.min(self.max_scroll());
            self.move_cursor(0);
        }
    }

    /// Recompute matches if a search is active, else clear. Called after any
    /// change to the document (reload, prefetch fill).
    fn refresh_search(&mut self) {
        if self.query.is_empty() {
            self.matches.clear();
            self.match_i = None;
        } else {
            self.compute_matches();
        }
    }

    /// Scan the whole document for `self.query` as a regex, filling
    /// `self.matches` with (row, cell_start, cell_end) hits. Smart-case: an
    /// all-lowercase pattern matches case-insensitively, any uppercase makes it
    /// case-sensitive. An invalid pattern yields no matches (the footer just
    /// shows "no matches" until it parses).
    fn compute_matches(&mut self) {
        let ci = !self.query.chars().any(|c| c.is_uppercase());
        let re = match RegexBuilder::new(&self.query).case_insensitive(ci).build() {
            Ok(re) => re,
            Err(_) => {
                self.matches.clear();
                self.match_i = None;
                return;
            }
        };
        let mut out = Vec::new();
        for (ri, row) in self.rows().iter().enumerate() {
            for (start, end) in match_cells(&row.content, &re) {
                out.push((ri, start, end));
            }
        }
        self.matches = out;
        self.match_i = None;
    }

    /// Jump to the first match at or after the cursor (wrapping), used right
    /// after confirming a query.
    fn search_to_first(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let i = self
            .matches
            .iter()
            .position(|&(r, _, _)| r >= self.cursor)
            .unwrap_or(0);
        self.goto_match(i);
    }

    /// Step to the next/prev match, wrapping around the ends.
    fn step_match(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as isize;
        let cur = self.match_i.unwrap_or(0) as isize;
        let i = ((cur + delta) % n + n) % n;
        self.goto_match(i as usize);
    }

    fn goto_match(&mut self, i: usize) {
        if let Some(&(row, cs, ce)) = self.matches.get(i) {
            self.match_i = Some(i);
            self.set_cursor(row);
            // Pan horizontally so the hit is on screen.
            let vw = self.content_view_w() as usize;
            self.hscroll = reveal(self.hscroll, cs, ce, vw).min(self.max_hscroll());
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
        // Watch is meaningless for a piped diff (pager mode): the input is a
        // one-shot static stream with no repo to watch. Force it off so `-w`
        // piped doesn't leave a stuck, un-toggleable indicator.
        self.watch = watch && !matches!(self.source, Source::Stdin);
        let mut last_poll = Instant::now();
        loop {
            // Fold in any rows the startup worker has finished.
            self.drain_prefetch();
            // Fold in a finished async worktree poll (non-blocking).
            self.drain_poll();
            // Swap in a finished async document rebuild (non-blocking).
            self.drain_rebuild();
            // Expire a transient footer note after its lifetime.
            if self.flash.as_ref().is_some_and(|(_, t)| t.elapsed() >= FLASH) {
                self.flash = None;
            }
            // Catch unstaged edits the git-internals watcher can't see. The
            // diff itself runs on a background thread so the loop never blocks.
            if last_poll.elapsed() >= poll {
                self.spawn_poll();
                last_poll = Instant::now();
            }
            self.render()?;
            // Poll faster while the prefetch is still streaming so freshly
            // parsed files appear promptly; idle at 200ms once it's done.
            let mut timeout = if self.prefetch.is_some()
                || self.poll_worker.is_some()
                || self.rebuild_worker.is_some()
            {
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
            // Keep the mascot animating smoothly (breathing, drift, poke)
            // whenever it's on screen — idle, or pinned over a diff.
            if self.files.is_empty() || self.mascot_pinned {
                timeout = timeout.min(Duration::from_millis(80));
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
    /// index or refs and so escape the git-internals watcher. The git call runs
    /// on a background thread (`poll_worker`) so a slow diff on a large repo
    /// never stalls rendering or input. One poll is in flight at a time; the
    /// next spawns only after the previous result is drained. Only active for
    /// worktree sources in watch mode.
    fn spawn_poll(&mut self) {
        if !self.watch || !self.source.reads_worktree() || self.poll_worker.is_some() {
            return;
        }
        let source = self.source.clone();
        let opts = self.effective_opts();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            if let Ok(text) = source.diff(&opts) {
                let _ = tx.send(text);
            }
        });
        self.poll_worker = Some(rx);
    }

    /// Fold in a finished async poll: rebuild only when the diff text actually
    /// changed, so idle ticks with no edits are free.
    fn drain_poll(&mut self) {
        let Some(rx) = self.poll_worker.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(text) => {
                if text != self.last_diff {
                    self.rebuild_from(text);
                }
            }
            Err(TryRecvError::Empty) => self.poll_worker = Some(rx), // still running
            Err(TryRecvError::Disconnected) => {} // finished (diff errored): drop rx
        }
    }

    /// Handle one event. Returns Ok(true) when the app should quit.
    fn handle(&mut self, ev: Event) -> io::Result<bool> {
        let page = self.viewport_rows() as isize;
        match ev {
            Event::KeyPress(k) => {
                // Ctrl+Z suspends to the shell like any pager; uncurses restores
                // the terminal, raises SIGTSTP, and resumes+repaints on `fg`.
                // Unix-only: Windows has no SIGTSTP / job control.
                #[cfg(unix)]
                if k.matches("ctrl+z") {
                    self.screen.suspend()?;
                    self.screen.resume()?;
                    return Ok(false);
                }
                // Search prompt captures the keyboard while typing: printable
                // keys extend the query, Enter confirms and jumps, Esc cancels,
                // Backspace edits. Nothing else fires until the prompt closes.
                if let Some(mut buf) = self.input.take() {
                    if k.matches("escape") {
                        // restore pre-search view
                        if let Some((cur, scr, q)) = self.search_return.take() {
                            self.query = q;
                            self.cursor = cur;
                            self.scroll = scr;
                            self.refresh_search();
                        }
                    } else if k.matches("enter") {
                        self.search_return = None;
                    } else {
                        if k.matches("backspace") {
                            buf.pop();
                        } else if let Some(t) = typed_text(&k) {
                            buf.push_str(&t);
                        } else {
                            self.input = Some(buf); // ignore non-text keys, stay open
                            return Ok(false);
                        }
                        // Live incsearch: recompute and jump on every edit,
                        // restoring the origin first so each match is found
                        // relative to where the search started.
                        self.input = Some(buf.clone());
                        self.query = buf;
                        if let Some((cur, scr, _)) = self.search_return {
                            self.cursor = cur;
                            self.scroll = scr;
                        }
                        if self.query.is_empty() {
                            self.matches.clear();
                            self.match_i = None;
                        } else {
                            self.compute_matches();
                            self.search_to_first();
                        }
                    }
                    return Ok(false);
                }
                // Escape closes transient UI in priority order: an active
                // selection first, then an active search, then the stat modal.
                // It never quits and never touches help (help is a toggle-only
                // inline footer, closed with `?`).
                if k.matches("escape") {
                    // Three Esc taps in quick succession (<600ms apart) toggle
                    // the mascot on over any view — even when there are changes.
                    let now = Instant::now();
                    let recent = self.esc_taps.1.is_some_and(|t| now.duration_since(t) < Duration::from_millis(600));
                    self.esc_taps = (if recent { self.esc_taps.0 + 1 } else { 1 }, Some(now));
                    if self.esc_taps.0 >= 3 {
                        self.mascot_pinned = !self.mascot_pinned;
                        self.esc_taps = (0, None);
                        return Ok(false);
                    }
                    if self.sel.is_some() {
                        self.sel = None;
                    } else if !self.query.is_empty() {
                        self.query.clear();
                        self.matches.clear();
                        self.match_i = None;
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
                    } else if k.matches_any(["j", "down"]) {
                        self.select_file(1);
                    } else if k.matches_any(["k", "up"]) {
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
                } else if k.matches_any(["h", "left"]) {
                    self.scroll_h(-4);
                } else if k.matches_any(["l", "right"]) {
                    self.scroll_h(4);
                } else if k.matches_any(["ctrl+d", "pagedown", "space"]) {
                    self.scroll_page(page / 2);
                } else if k.matches_any(["ctrl+u", "pageup"]) {
                    self.scroll_page(-page / 2);
                } else if k.matches_any(["g", "home"]) {
                    self.cursor_to(0);
                } else if k.matches_any(["G", "end"]) {
                    self.cursor_to(self.rows().len().saturating_sub(1));
                } else if k.matches_any(["}", ")"]) {
                    self.jump_hunk(1);
                } else if k.matches_any(["{", "("]) {
                    self.jump_hunk(-1);
                } else if k.matches("/") {
                    self.search_return = Some((self.cursor, self.scroll, self.query.clone()));
                    self.input = Some(String::new());
                } else if k.matches("n") {
                    self.step_match(1);
                } else if k.matches("N") {
                    self.step_match(-1);
                } else if k.matches_any(["tab", "]"]) {
                    self.select_file(1);
                } else if k.matches_any(["shift+tab", "["]) {
                    self.select_file(-1);
                } else if k.matches("s") {
                    self.split = !self.split;
                } else if k.matches("f") {
                    self.view = View::Stat;
                } else if k.matches("b") {
                    self.sidebar = Some(!self.sidebar_visible());
                } else if k.matches("w") {
                    // Watch needs a live repo; a piped diff (pager mode) is
                    // static, so leave it a no-op there.
                    if !matches!(self.source, Source::Stdin) {
                        self.watch = !self.watch;
                        // Turning watch on catches up on anything that changed
                        // while it was off: notifications received meanwhile were
                        // drained and dropped, so without this the view stays
                        // stale until the next change. (The `a` toggle reloads on
                        // flip too.)
                        if self.watch {
                            self.reload();
                        }
                    }
                } else if k.matches("a") {
                    // Toggle untracked files in the worktree view; a no-op for
                    // rev/staged/stdin sources, which never surface untracked.
                    if self.source.reads_worktree() {
                        self.opts.all = !self.opts.all;
                        self.reload();
                    }
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
                match m.button {
                    MouseButton::WheelUp => self.scroll_page(-3),
                    MouseButton::WheelDown => self.scroll_page(3),
                    MouseButton::WheelLeft => self.scroll_h(-3),
                    MouseButton::WheelRight => self.scroll_h(3),
                    _ => {}
                }
            }
            Event::MouseClick(m) => {
                if m.button != MouseButton::Left {
                    return Ok(false);
                }
                let footer_row = self.body_h_screen() as u16;
                // The "? help" badge toggles the help grid from any view.
                if m.y == footer_row && m.x >= self.help_badge_x {
                    self.help_open = !self.help_open;
                    return Ok(false);
                }
                // The "diffv" logo badge toggles the mascot from any view
                // (same as tapping Esc three times).
                if m.y == footer_row && m.x < self.width(" diffv ") {
                    self.mascot_pinned = !self.mascot_pinned;
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
                    // The mascot is clickable whenever it's on screen (idle
                    // screen, or pinned over a diff): a hit pokes it and starts
                    // a drag. On the idle screen nothing else is clickable.
                    if m.y < footer_row {
                        let bx = self.body_x();
                        if let Some(mascot) = self.mascot.as_mut() {
                            let (x0, y0, x1, y1) = mascot.bbox();
                            let cx = m.x.saturating_sub(bx);
                            if cx >= x0 && cx < x1 && m.y >= y0 && m.y < y1 {
                                mascot.poke();
                                mascot.grab();
                                self.mascot_grab = Some((cx - x0, m.y - y0));
                                return Ok(false);
                            }
                        }
                    }
                    if self.files.is_empty() {
                        return Ok(false);
                    }
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
                        self.set_cursor(self.screen_y_to_doc(m.y));
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
                if let Some((gx, gy)) = self.mascot_grab {
                    // Dragging the idle mascot: follow the pointer, keeping the
                    // grab offset, and let it lean its antennas as it moves.
                    let bx = self.body_x();
                    let body_h = self.body_h_screen() as u16;
                    let bw = self.screen.width().saturating_sub(self.sidebar_w());
                    let tx = m.x.saturating_sub(bx).saturating_sub(gx) as f32;
                    let ty = m.y.saturating_sub(gy) as f32;
                    if let Some(mascot) = self.mascot.as_mut() {
                        mascot.drag_to(tx, ty, bw, body_h);
                    }
                } else if let Some((gx, gy)) = self.modal_drag {
                    self.modal_pos = Some((m.x.saturating_sub(gx), m.y.saturating_sub(gy)));
                } else if self.resizing {
                    self.resize_sidebar_to(m.x);
                } else if self.resizing_split {
                    self.resize_split_to(m.x);
                } else if self.sel.is_some_and(|s| s.dragging) {
                    let body_h = self.body_h_screen() as u16;
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
                    if self.mascot_grab.take().is_some() {
                        if let Some(mascot) = self.mascot.as_mut() {
                            mascot.release();
                        }
                    }
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
                self.scroll_h(0);
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
        let sticky = self.sticky_rows();
        let k = sticky.len() as u16;
        let hs = self.hscroll as u16;
        let (sr, sc, er, ec) = sel.ordered();
        // Compute the on-screen highlight span for each visible selected row up
        // front, so the immutable row borrow is released before we touch cells.
        let mut segs: Vec<(u16, u16, u16)> = Vec::new();
        {
            let rows = self.rows();
            let er = er.min(rows.len().saturating_sub(1));
            for r in sr..=er {
                // A selected row shows either pinned in the sticky band or in
                // the scrolled content window; otherwise it's off-screen.
                let y = if let Some(p) = sticky.iter().position(|&s| s == r) {
                    p as u16
                } else if r >= scroll && r < scroll + body_h {
                    k + (r - scroll) as u16
                } else {
                    continue;
                };
                let row = &rows[r];
                if !App::row_in_pane(row.kind, sel.pane) {
                    continue;
                }
                let (origin, cstart) = self.pane_geom(row.kind, sel.pane);
                let cs = origin + cstart;
                let len = row.content.len() as u16;
                let start = if r == sr { sc as u16 } else { 0 };
                let end = if r == er { ec as u16 } else { len };
                // Map content columns to screen columns through the horizontal
                // scroll: anything left of `hscroll` is off-screen.
                let s0 = start.min(len).max(hs);
                let e0 = end.min(len).max(hs);
                let sx = (cs + (s0 - hs)).min(right);
                let ex = (cs + (e0 - hs)).min(right);
                if ex > sx {
                    segs.push((y, sx, ex));
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

    /// Overlay search-match highlights on the finished frame: every visible hit
    /// gets `search_match`, the current hit `search_current`. In split view a
    /// hit is painted in each pane its row shows (both sides for context, one
    /// side for add/remove), clamped to that pane like the selection.
    fn paint_matches(&mut self) {
        if self.query.is_empty() || self.matches.is_empty() || self.rows().is_empty() {
            return;
        }
        let w = self.screen.width();
        let sw = self.sidebar_w();
        let body_right = if self.sidebar_left() { w } else { w - sw };
        let body_h = self.viewport_rows();
        let scroll = self.scroll;
        let sticky = self.sticky_rows();
        let k = sticky.len() as u16;
        let hs = self.hscroll as u16;
        let split = self.split;
        let div = self.split_div_x();
        // (y, sx, ex, current) computed while rows are borrowed immutably.
        let mut segs: Vec<(u16, u16, u16, bool)> = Vec::new();
        {
            let rows = self.rows();
            for (mi, &(r, cstart, cend)) in self.matches.iter().enumerate() {
                if r >= rows.len() {
                    continue;
                }
                let y = if let Some(p) = sticky.iter().position(|&s| s == r) {
                    p as u16
                } else if r >= scroll && r < scroll + body_h {
                    k + (r - scroll) as u16
                } else {
                    continue;
                };
                let kind = rows[r].kind;
                let len = rows[r].content.len() as u16;
                let cur = self.match_i == Some(mi);
                let panes: &[Option<Pane>] =
                    if !split || matches!(kind, RowKind::Hunk | RowKind::Note) {
                        &[None]
                    } else {
                        &[Some(Pane::Left), Some(Pane::Right)]
                    };
                for &pane in panes {
                    if pane.is_some() && !App::row_in_pane(kind, pane) {
                        continue;
                    }
                    let (origin, cs) = self.pane_geom(kind, pane);
                    let base = origin + cs;
                    let right = if pane == Some(Pane::Left) {
                        div.min(body_right)
                    } else {
                        body_right
                    };
                    let sx = (base + (cstart as u16).min(len).max(hs) - hs).min(right);
                    let ex = (base + (cend as u16).min(len).max(hs) - hs).min(right);
                    if ex > sx {
                        segs.push((y, sx, ex, cur));
                    }
                }
            }
        }
        let (mstyle, cstyle) = (self.theme.search_match.clone(), self.theme.search_current.clone());
        for (y, sx, ex, cur) in segs {
            let st = if cur { &cstyle } else { &mstyle };
            for x in sx..ex {
                if let Some(c) = self.screen.cell_mut((x, y)) {
                    c.style = st.clone();
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
        let empty = self.files.is_empty();
        if empty {
            // Body is blank; the mascot is painted last, above everything.
        } else if self.split {
            self.render_split(bx, bw, body_h);
        } else {
            self.render_diff(bx, bw, body_h);
        }
        if !empty && !self.mascot_pinned {
            self.mascot = None;
            self.mascot_grab = None;
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
        // The stat modal floats above the footer and help.
        if self.view == View::Stat {
            self.render_stat_modal();
        }
        // Overlay search-match highlights, then the selection on top.
        self.paint_matches();
        if let Some(sel) = self.sel {
            self.paint_selection(sel);
        }
        // The mascot floats above absolutely everything — including selection
        // and search highlights — so those overlays never recolour it.
        if empty || self.mascot_pinned {
            self.render_empty(bx, bw, body_h);
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

        // "A" badge when the worktree view includes untracked files.
        let a_x = if self.opts.all && self.source.reads_worktree() {
            let a_badge = " A ";
            let ax = w_x.saturating_sub(self.width(a_badge));
            self.screen
                .set_str((ax, row), a_badge, self.theme.statusbar_watch.clone());
            ax
        } else {
            w_x
        };

        // Global stats.
        let stats = format!(" {nf} files +{add} -{del} ");
        let stats_x = a_x.saturating_sub(self.width(&stats));
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

        // Search badge sits right after the per-file stats: the prompt while
        // typing, else the match counter once confirmed. Secondary fg, no
        // distinct background. It advances `x` so it never overlaps the flash
        // note below.
        let search_badge = if let Some(buf) = &self.input {
            Some(format!(" /{buf} "))
        } else if !self.query.is_empty() {
            Some(if self.matches.is_empty() {
                " no matches ".to_string()
            } else {
                let n = self.match_i.map(|i| i + 1).unwrap_or(0);
                format!(" [{}/{}] ", n, self.matches.len())
            })
        } else {
            None
        };
        if let Some(badge) = search_badge {
            if x < stats_x {
                let (badge, bw) = self.clip(&badge, stats_x - x);
                self.screen
                    .set_str((x, row), &badge, self.theme.statusbar_search.clone());
                x += bw;
            }
        }
        // Transient note (e.g. "copied 3 lines") auto-expires; it renders after
        // the search badge so an active search never suppresses it.
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
        let entries = self.help_entries();
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

    /// Idle screen: no changes, so float the mascot around the empty body. The
    /// mascot breathes, reacts to pokes, and can be dragged (see the mouse
    /// handlers).
    fn render_empty(&mut self, bx: u16, bw: u16, body_h: u16) {
        // Faint centered "clean" note, only on the real empty screen (not when
        // the mascot is pinned over a diff). Drawn first so the mascot floats
        // over it.
        if self.files.is_empty() {
            let hint = "working tree clean";
            let hw = self.width(hint);
            if bw >= hw && body_h >= 1 {
                let hx = bx + (bw - hw) / 2;
                self.screen
                    .set_str((hx, body_h - 1), hint, self.theme.context.clone().faint());
            }
        }
        if bw < MASCOT_W || body_h < MASCOT_H {
            self.mascot = None;
            return;
        }
        let m = self.mascot.get_or_insert_with(|| Mascot::new(bw, body_h));
        m.tick(bw, body_h);

        let body = self.theme.mascot_body;
        let accent = self.theme.mascot_accent;
        let face_fg = Color::Black; // eyes are always black
        let flashing = m.flashing();
        let mat = |c: u8| -> Option<Color> {
            match c {
                b'B' => Some(if flashing { accent } else { body }),
                b'A' => Some(if flashing { body } else { accent }),
                b'D' => Some(face_fg),
                _ => None,
            }
        };
        let grid = m.frame();
        let (ox, oy) = (m.x.round() as u16, m.y.round() as u16);

        // Each rendered cell packs two vertical pixels via a half-block: the
        // upper pixel is the fg of `▀`, the lower is its bg (or `▄`/space when
        // one side is empty/solid). The whole creature — body, antennas, and
        // face — is drawn this way, so there are no letter glyphs. When a half
        // is transparent, we keep whatever background is already on the cell so
        // the mascot floats over the diff without punching holes in it.
        for r in 0..MASCOT_H as usize {
            let top = grid.get(2 * r);
            let bot = grid.get(2 * r + 1);
            for col in 0..MASCOT_W as usize {
                let tc = top.and_then(|row| mat(row[col]));
                let bc = bot.and_then(|row| mat(row[col]));
                let (px, py) = (bx + ox + col as u16, oy + r as u16);
                // The cell's *visible* background: a reversed cell (e.g. under a
                // selection) shows its fg as background, so swap when REVERSE is
                // set. This keeps the mascot's transparent halves matching what's
                // actually drawn behind it.
                let under = self.screen.cell_mut((px, py)).and_then(|c| {
                    if c.style.attrs.contains(AttrFlags::REVERSE) {
                        c.style.fg
                    } else {
                        c.style.bg
                    }
                });
                let (glyph, style) = match (tc, bc) {
                    (None, None) => continue,
                    (Some(c), None) => ("▀", Style::default().fg(c).bg(under)),
                    (None, Some(c)) => ("▄", Style::default().fg(c).bg(under)),
                    (Some(a), Some(b)) if a == b => (" ", Style::default().bg(a)),
                    (Some(a), Some(b)) => ("▀", Style::default().fg(a).bg(b)),
                };
                self.screen.set_str((px, py), glyph, style);
            }
        }
    }

    fn render_diff(&mut self, x: u16, width: u16, body_h: u16) {
        // The sticky band pins the enclosing file/hunk headers to the top; the
        // scrolled content follows below it.
        let sticky = self.sticky_rows();
        // Move the document rows out so we can freely borrow `self.screen`
        // while iterating; restored right after rendering.
        let rows = std::mem::take(&mut self.doc_rows);
        for row in 0..body_h {
            let idx = if (row as usize) < sticky.len() {
                sticky[row as usize]
            } else {
                self.scroll + (row as usize - sticky.len())
            };
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
                    RowKind::Hunk | RowKind::File => self.theme.header_bg,
                    _ => None,
                }
            };
            self.draw_diff_row(r, x, width, y, row_bg, Gut::Both);
        }
        // Restore the rows we borrowed.
        self.doc_rows = rows;
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
        let sticky = self.sticky_rows();
        let rows = std::mem::take(&mut self.doc_rows);
        for row in 0..body_h {
            let idx = if (row as usize) < sticky.len() {
                sticky[row as usize]
            } else {
                self.scroll + (row as usize - sticky.len())
            };
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
                RowKind::File => {
                    let hbg = cbg.or(self.theme.header_bg);
                    self.draw_diff_row(r, x, width, y, hbg, Gut::Both);
                    continue;
                }
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
        self.doc_rows = rows;
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
            RowKind::File => {
                let (s, _) = self.slice_h(&r.spans[0].text, self.hscroll as u16, width);
                self.screen
                    .set_str((cx, y), &s, bg(self.theme.header.clone().bold()));
                return;
            }
            RowKind::Hunk => {
                let (s, _) = self.slice_h(&r.spans[0].text, self.hscroll as u16, width);
                self.screen
                    .set_str((cx, y), &s, bg(self.theme.header.clone()));
                return;
            }
            RowKind::Note => {
                let (s, _) = self.slice_h(&r.spans[0].text, self.hscroll as u16, width);
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
        let content_x0 = cx;
        let hs = self.hscroll as u16;
        // Content column where the current span begins. The gutter and sign are
        // pinned; only the spans past `content_x0` shift left by `hscroll`.
        let mut vcol: u16 = 0;
        for span in &r.spans {
            let spanw = self.width(&span.text);
            let screen_x = content_x0 + vcol.saturating_sub(hs);
            if screen_x >= avail {
                break;
            }
            let skip = hs.saturating_sub(vcol);
            if skip >= spanw {
                vcol += spanw;
                continue;
            }
            let remaining = avail - screen_x;
            let (text, _) = self.slice_h(&span.text, skip, remaining);
            vcol += spanw;
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
            self.screen.set_str((screen_x, y), &text, bg(style));
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

    /// Like [`Self::clip`] but first drops `skip` leading display columns, so a
    /// line can be scrolled horizontally. A wide cluster straddling the left
    /// edge is dropped whole (never split). Returns the slice and its width; at
    /// `skip == 0` it is identical to `clip`.
    fn slice_h(&self, s: &str, skip: u16, width: u16) -> (String, u16) {
        slice_fit(self.screen.grapheme_cells(s), skip, width)
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
/// The file index owning document row `row`, given each file's start offset.
/// Rows before the first start (there are none in practice) map to file 0.
fn file_of_row(starts: &[usize], row: usize) -> usize {
    starts.partition_point(|&s| s <= row).saturating_sub(1)
}

/// Document rows to pin at the top of the body for a given `scroll`: the
/// enclosing file header and the current hunk header, but only once they've
/// scrolled strictly above the top content line (so a header still naturally on
/// screen isn't duplicated). Capped so at least one content row remains.
fn sticky_at(
    kinds: &[RowKind],
    file_starts: &[usize],
    scroll: usize,
    body_h: usize,
) -> Vec<usize> {
    if scroll == 0 || scroll >= kinds.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let fi = file_starts.get(file_of_row(file_starts, scroll)).copied().unwrap_or(0);
    if fi < scroll {
        out.push(fi);
    }
    // The hunk to pin is the one whose body contains the top line. If the top
    // line *is* a hunk header it's already visible, so nothing to pin.
    if let Some(hi) = (fi + 1..=scroll).rev().find(|&i| kinds[i] == RowKind::Hunk) {
        if hi < scroll {
            out.push(hi);
        }
    }
    out.truncate(body_h.saturating_sub(1));
    out
}

/// A finished background document rebuild, swapped in atomically by
/// `drain_rebuild` so the old document stays interactive until it's ready.
struct Rebuilt {
    files: Vec<FileDiff>,
    raw: Vec<String>,
    doc: Vec<Row>,
    starts: Vec<usize>,
}

/// A semantic bookmark of the cursor position, resilient to a document being
/// rebuilt: the file (by path), what to home in on within it, and how far down
/// the screen the cursor was.
struct Anchor {
    path: String,
    target: AnchorTarget,
    screen_off: usize,
}

enum AnchorTarget {
    /// A content line, matched by new-side then old-side line number.
    Line(Option<usize>, Option<usize>),
    /// The nth (0-based) hunk header within the file.
    Hunk(usize),
    /// The file header (fallback when no line/hunk applies).
    Start,
}

/// Resolve a captured anchor to a row in a document. Falls back to the file's
/// header row (or row 0, if the file is gone) when the exact line or hunk can't
/// be found. Free-standing so the mapping is unit-testable.
fn resolve_anchor_row(files: &[FileDiff], starts: &[usize], doc: &[Row], a: &Anchor) -> usize {
    let Some(fi) = files.iter().position(|f| f.path() == a.path.as_str()) else {
        return 0;
    };
    let start = starts[fi];
    let end = starts.get(fi + 1).copied().unwrap_or(doc.len());
    let range = &doc[start..end];
    match &a.target {
        AnchorTarget::Start => start,
        AnchorTarget::Line(old, new) => {
            let by_new = new.and_then(|n| range.iter().position(|r| r.new_no == Some(n)));
            let by_old = old.and_then(|o| range.iter().position(|r| r.old_no == Some(o)));
            start + by_new.or(by_old).unwrap_or(0)
        }
        AnchorTarget::Hunk(ord) => range
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == RowKind::Hunk)
            .nth(*ord)
            .map(|(i, _)| start + i)
            .unwrap_or(start),
    }
}

/// Build the whole continuous document: each file's header row followed by its
/// body rows, recording each file's start offset. Free-standing so it can run
/// on a background rebuild thread.
fn assemble_document(
    files: &[FileDiff],
    hl: &Highlighter,
    intraline: bool,
    tab: usize,
) -> (Vec<Row>, Vec<usize>) {
    let mut doc = Vec::new();
    let mut starts = Vec::with_capacity(files.len());
    for f in files {
        starts.push(doc.len());
        doc.push(file_header_row(f));
        doc.extend(build_file_rows(f, hl, intraline, tab));
    }
    (doc, starts)
}

/// A `git diff`-style header row that introduces a file in the continuous
/// document (renames show both paths, matching real unified-diff output).
fn file_header_row(f: &FileDiff) -> Row {
    // Git's `diff --git` line always carries the real filename on both sides,
    // even for creates/deletes — the /dev/null sentinel only appears on the
    // ---/+++ lines. Fall back to the display path so a new file doesn't render
    // as "a//dev/null".
    let real = f.path();
    let a = if f.old_path == "/dev/null" || f.old_path.is_empty() { real } else { &f.old_path };
    let b = if f.new_path == "/dev/null" || f.new_path.is_empty() { real } else { &f.new_path };
    let text = format!("diff --git a/{a} b/{b}");
    Row::new(
        RowKind::File,
        None,
        None,
        vec![Span {
            fg: None,
            changed: false,
            text,
        }],
    )
}

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

/// New horizontal scroll that brings the column range `[cs, ce)` into a view
/// `vw` columns wide: scroll left if the hit starts before the view, right if
/// it runs past it (favouring the start when the hit is wider than the view).
/// Returns the current offset unchanged when the hit already fits on screen.
fn reveal(hscroll: usize, cs: usize, ce: usize, vw: usize) -> usize {
    if cs < hscroll {
        cs
    } else if ce > hscroll + vw {
        ce.saturating_sub(vw).min(cs)
    } else {
        hscroll
    }
}

/// Fit `(cluster, width)` pairs into at most `width` columns after skipping
/// `skip` leading columns, without splitting a wide cluster. A cluster that
/// straddles the `skip` boundary is dropped whole. Returns the fitted string
/// and the columns it occupies. At `skip == 0` this equals [`fit`].
fn slice_fit<'a>(cells: impl Iterator<Item = (&'a str, u8)>, skip: u16, width: u16) -> (String, u16) {
    let mut out = String::new();
    let mut col = 0u16;
    let mut w = 0u16;
    for (g, gw) in cells {
        let gw = gw as u16;
        if col < skip {
            col += gw;
            continue;
        }
        if w + gw > width {
            break;
        }
        out.push_str(g);
        w += gw;
        col += gw;
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
    use super::{expand_tabs, fit, fit_tail, file_of_row, match_cells, reveal, slice_cells, slice_fit, text_cells, Face, Mascot, Sel, Span, MASCOT_H, MASCOT_W};
    use regex::RegexBuilder;
    use uncurses::text::{grapheme_cells, WidthMode};

    // Exercise the same fitting logic clip() uses; clip() itself needs a live
    // screen, so we feed grapheme_cells directly here.
    fn clip(s: &str, width: u16) -> (String, u16) {
        fit(grapheme_cells(s, WidthMode::Grapheme, false), width)
    }

    fn slice_h(s: &str, skip: u16, width: u16) -> (String, u16) {
        slice_fit(grapheme_cells(s, WidthMode::Grapheme, false), skip, width)
    }

    #[test]
    fn mascot_leans_opposite_the_drag_and_stays_in_bounds() {
        let (bw, bh) = (40u16, 20u16);
        let mut m = Mascot::new(bw, bh);
        // Dragging right pushes lean negative, so the antennas trail left.
        m.grab();
        m.drag_to(m.x + 5.0, m.y, bw, bh);
        assert_eq!(m.lean_offset(), -1, "antennas trail left when dragged right");
        // Dragging left leans the other way.
        m.drag_to(m.x - 5.0, m.y, bw, bh);
        assert_eq!(m.lean_offset(), 1, "antennas trail right when dragged left");
        // A drag far past the edge is clamped inside the body.
        m.drag_to(1000.0, 1000.0, bw, bh);
        let (bx0, by0, bx1, by1) = m.bbox();
        assert!(bx1 <= bw && by1 <= bh, "clamped in bounds: {bx0},{by0},{bx1},{by1}");
        assert_eq!(bx1 - bx0, MASCOT_W);
        assert_eq!(by1 - by0, MASCOT_H);
    }

    #[test]
    fn mascot_face_reacts_to_drag_and_poke() {
        let mut m = Mascot::new(40, 20);
        m.grab();
        assert_eq!(m.face(), Face::Dizzy, "dragging looks dizzy");
        m.release();
        m.poke();
        assert_eq!(m.face(), Face::Surprised, "a fresh poke looks surprised");
    }

    #[test]
    fn reveal_pans_to_show_a_match() {
        // Already visible: unchanged.
        assert_eq!(reveal(0, 5, 10, 72), 0);
        assert_eq!(reveal(4, 5, 10, 72), 4);
        // Hit before the view: scroll left to its start.
        assert_eq!(reveal(20, 5, 10, 72), 5);
        // Hit past the right edge: scroll right just enough to show its end.
        assert_eq!(reveal(0, 100, 108, 72), 108 - 72);
        // Hit wider than the view: favour showing its start.
        assert_eq!(reveal(0, 100, 200, 72), 100);
    }

    #[test]
    fn slice_h_skips_columns_and_matches_clip_at_zero() {
        // At skip 0 it must be byte-identical to clip.
        assert_eq!(slice_h("hello world", 0, 5), clip("hello world", 5));
        // Skipping drops leading columns, then fits the budget.
        assert_eq!(slice_h("hello world", 6, 5), ("world".into(), 5));
        assert_eq!(slice_h("hello world", 3, 4), ("lo w".into(), 4));
        // Skipping past the end yields nothing.
        assert_eq!(slice_h("abc", 10, 5), (String::new(), 0));
        // A wide cluster straddling the skip boundary is dropped whole, never
        // split: "世界" is two 2-col clusters, skip=1 lands mid-"世".
        assert_eq!(slice_h("世界", 1, 4), ("界".into(), 2));
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

    #[test]
    fn file_header_uses_real_name_for_new_and_deleted_files() {
        use crate::diff::FileDiff;
        let fd = |old: &str, new: &str| FileDiff {
            old_path: old.into(),
            new_path: new.into(),
            hunks: vec![],
            is_binary: false,
            notes: vec![],
        };
        let text = |f: &FileDiff| super::file_header_row(f).spans.iter().map(|s| s.text.clone()).collect::<String>();
        // New file: old side is /dev/null, but the header must show the real name.
        assert_eq!(text(&fd("/dev/null", "new.txt")), "diff --git a/new.txt b/new.txt");
        // Deleted file: new side is /dev/null.
        assert_eq!(text(&fd("old.txt", "/dev/null")), "diff --git a/old.txt b/old.txt");
        // Rename keeps both distinct paths.
        assert_eq!(text(&fd("from.txt", "to.txt")), "diff --git a/from.txt b/to.txt");
    }

    #[test]
    fn file_of_row_maps_document_rows_to_files() {
        // Three files whose headers sit at rows 0, 5, 12.
        let starts = [0usize, 5, 12];
        assert_eq!(file_of_row(&starts, 0), 0); // first header
        assert_eq!(file_of_row(&starts, 4), 0); // last row of file 0
        assert_eq!(file_of_row(&starts, 5), 1); // exactly the next header
        assert_eq!(file_of_row(&starts, 11), 1);
        assert_eq!(file_of_row(&starts, 12), 2); // last file's header
        assert_eq!(file_of_row(&starts, 99), 2); // past the end stays in last
    }

    #[test]
    fn sticky_at_pins_file_and_hunk_headers_above_scroll() {
        use super::{sticky_at, RowKind::*};
        // File header, hunk, some lines, second hunk, more lines.
        let kinds = [File, Hunk, Context, Add, Hunk, Context, Context, Context];
        let starts = [0usize];
        let big = 100; // roomy body: nothing gets truncated
        // At the very top nothing is pinned.
        assert_eq!(sticky_at(&kinds, &starts, 0, big), Vec::<usize>::new());
        // Scrolled just past the file header: only the file header pins (the
        // first hunk at row 1 is still the top visible line, not above it).
        assert_eq!(sticky_at(&kinds, &starts, 1, big), vec![0]);
        // Inside the first hunk: file header + that hunk header pin.
        assert_eq!(sticky_at(&kinds, &starts, 3, big), vec![0, 1]);
        // Past the second hunk header: it replaces the first as the current one.
        assert_eq!(sticky_at(&kinds, &starts, 6, big), vec![0, 4]);
        // Landing exactly on the second hunk header pins only the file header —
        // that hunk header is itself the top visible line, so it isn't pinned.
        assert_eq!(sticky_at(&kinds, &starts, 4, big), vec![0]);
        // A cramped body keeps at least one content row (cap = body_h - 1).
        assert_eq!(sticky_at(&kinds, &starts, 6, 2), vec![0]);
        assert_eq!(sticky_at(&kinds, &starts, 6, 1), Vec::<usize>::new());
    }

    #[test]
    fn sticky_at_second_file_ignores_earlier_files_headers() {
        use super::{sticky_at, RowKind::*};
        // Two files; the second starts at row 3.
        let kinds = [File, Hunk, Context, File, Hunk, Context, Context];
        let starts = [0usize, 3];
        // Inside the second file's hunk: pin that file's header (3) and hunk (4),
        // never the first file's rows.
        assert_eq!(sticky_at(&kinds, &starts, 6, 100), vec![3, 4]);
        // Exactly on the second file's header row: nothing pins yet.
        assert_eq!(sticky_at(&kinds, &starts, 3, 100), Vec::<usize>::new());
    }

    #[test]
    fn resolve_anchor_finds_line_hunk_and_fallbacks() {
        use super::{resolve_anchor_row, Anchor, AnchorTarget, Row, RowKind, Span};
        use crate::diff::FileDiff;
        fn fd(path: &str) -> FileDiff {
            FileDiff {
                old_path: path.into(),
                new_path: path.into(),
                hunks: vec![],
                is_binary: false,
                notes: vec![],
            }
        }
        fn row(kind: RowKind, old: Option<usize>, new: Option<usize>) -> Row {
            Row::new(kind, old, new, vec![Span { fg: None, changed: false, text: String::new() }])
        }
        // Two files: a.txt (rows 0..=4) and b.txt (rows 5..=9). Each has a File
        // header, a Hunk header, then three content lines.
        let files = [fd("a.txt"), fd("b.txt")];
        let starts = [0usize, 5];
        let doc = vec![
            row(RowKind::File, None, None),         // 0
            row(RowKind::Hunk, None, None),         // 1
            row(RowKind::Context, Some(1), Some(1)), // 2
            row(RowKind::Remove, Some(2), None),    // 3
            row(RowKind::Add, None, Some(2)),       // 4
            row(RowKind::File, None, None),         // 5
            row(RowKind::Hunk, None, None),         // 6
            row(RowKind::Context, Some(1), Some(1)), // 7
            row(RowKind::Hunk, None, None),         // 8
            row(RowKind::Add, None, Some(9)),       // 9
        ];
        let anchor = |path: &str, target| Anchor { path: path.into(), target, screen_off: 0 };
        // A new-side line number resolves within its file.
        assert_eq!(resolve_anchor_row(&files, &starts, &doc, &anchor("b.txt", AnchorTarget::Line(None, Some(9)))), 9);
        // A removed line (no new_no) falls back to old_no.
        assert_eq!(resolve_anchor_row(&files, &starts, &doc, &anchor("a.txt", AnchorTarget::Line(Some(2), None))), 3);
        // The nth hunk within a file (b.txt's second hunk is doc row 8).
        assert_eq!(resolve_anchor_row(&files, &starts, &doc, &anchor("b.txt", AnchorTarget::Hunk(1))), 8);
        // Start anchors to the file's header row.
        assert_eq!(resolve_anchor_row(&files, &starts, &doc, &anchor("b.txt", AnchorTarget::Start)), 5);
        // An unmatched line falls back to the file header.
        assert_eq!(resolve_anchor_row(&files, &starts, &doc, &anchor("a.txt", AnchorTarget::Line(None, Some(999)))), 0);
        // A vanished file falls back to the top of the document.
        assert_eq!(resolve_anchor_row(&files, &starts, &doc, &anchor("gone.txt", AnchorTarget::Start)), 0);
    }

    #[test]
    fn search_maps_matches_to_columns() {
        let re = |p: &str, ci: bool| RegexBuilder::new(p).case_insensitive(ci).build().unwrap();
        // Substring hits map to (start_col, end_col) cell ranges, one per
        // occurrence. "o" appears twice in "hello world".
        let cells = text_cells("hello world");
        assert_eq!(match_cells(&cells, &re("o", false)), vec![(4, 5), (7, 8)]);
        // Smart-case: case-insensitive matches either case.
        let cells = text_cells("Foo BAR foo");
        assert_eq!(match_cells(&cells, &re("foo", true)), vec![(0, 3), (8, 11)]);
        // Case-sensitive only hits the exact case.
        assert_eq!(match_cells(&cells, &re("foo", false)), vec![(8, 11)]);
        // Regex metacharacters work: `\w+` spans each run of word chars.
        assert_eq!(match_cells(&cells, &re(r"\w+", false)), vec![(0, 3), (4, 7), (8, 11)]);
        // Wide chars: a match after a 2-column glyph lands on the right columns
        // and its end covers the full width of a wide match.
        let cells = text_cells("世x界");
        assert_eq!(match_cells(&cells, &re("x", false)), vec![(2, 3)]);
        assert_eq!(match_cells(&cells, &re("界", false)), vec![(3, 5)]);
        // No match; zero-width matches are skipped.
        assert!(match_cells(&cells, &re("z", false)).is_empty());
        assert!(match_cells(&cells, &re("", false)).is_empty());
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
