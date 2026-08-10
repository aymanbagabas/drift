//! Configuration: theme, colors, and settings loaded from a TOML/YAML/JSON
//! file and overlaid with any `[drift]` values from git config.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use uncurses::color::Color;
use uncurses::style::Style;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// syntect theme name, e.g. "base16-ocean.dark".
    pub theme: String,
    /// Enable syntax highlighting of diff content.
    pub syntax: bool,
    /// Enable intra-line (word-level) change highlighting.
    pub intraline: bool,
    /// Show old/new line numbers in the gutter.
    pub line_numbers: bool,
    /// Spaces per tab when rendering.
    pub tab_width: usize,
    /// Sidebar visibility: "auto" (open when terminal >= 150 wide, default),
    /// "always" (open), or "never" (closed). The `b` key overrides at runtime.
    pub sidebar: String,
    /// Width in cells of the file-list sidebar (including its divider). A mouse
    /// drag on the divider overrides this at runtime.
    pub sidebar_width: usize,
    /// Which side the sidebar sits on: "left" (default) or "right".
    pub sidebar_side: String,
    /// Editor command; falls back to $EDITOR / $VISUAL when empty.
    pub editor: String,
    pub colors: Colors,
    /// Component styles, each a `fg bg attrs...` spec resolved against the
    /// color palette. Overrides the built-in defaults per named component.
    #[serde(default)]
    pub styles: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Colors {
    pub add: String,
    pub remove: String,
    pub context: String,
    pub header: String,
    pub line_number: String,
    /// Primary accent: the "drift" badge background in the footer.
    pub primary: String,
    /// Secondary accent: file name text, the "? help" badge, and dialog
    /// backgrounds.
    pub secondary: String,
    /// Bright body text (the "white" tone).
    pub foreground: String,
    /// Text drawn on top of accent badges and dialog surfaces (the "black"
    /// tone).
    pub background: String,
    /// Muted / dim tone: flags, help descriptions.
    pub muted: String,
    /// Status bar and chip background surface.
    pub surface: String,
    /// Current-line highlight background.
    pub cursor: String,
    /// Subtle whole-line background for added/removed lines.
    pub add_line: String,
    pub remove_line: String,
    /// Spanning background band behind a hunk header (section separator).
    pub header_line: String,
    /// Background emphasis for intra-line changed segments.
    pub add_emph: String,
    pub remove_emph: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: String::new(),
            syntax: true,
            intraline: true,
            line_numbers: true,
            tab_width: 4,
            sidebar: "auto".to_string(),
            sidebar_width: 30,
            sidebar_side: "left".to_string(),
            editor: String::new(),
            colors: Colors::default(),
            styles: HashMap::new(),
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        // Empty means "unset": resolved from the built-in onedark/onelight
        // palette (by terminal background) after loading, so explicit config
        // and gitconfig overrides win.
        Colors {
            add: String::new(),
            remove: String::new(),
            context: String::new(),
            header: String::new(),
            line_number: String::new(),
            primary: String::new(),
            secondary: String::new(),
            foreground: String::new(),
            background: String::new(),
            muted: String::new(),
            surface: String::new(),
            cursor: String::new(),
            add_line: String::new(),
            remove_line: String::new(),
            header_line: String::new(),
            add_emph: String::new(),
            remove_emph: String::new(),
        }
    }
}

impl Config {
    /// Load config: defaults, then a config file (explicit path or the first
    /// found in standard locations), then git config `[drift]` overrides.
    /// Colors and styles keep their unset (empty) state — they're resolved
    /// against the theme's built-in defaults lazily, in `Palette`/`Theme`.
    pub fn load(explicit: Option<&Path>) -> Self {
        let mut cfg = Config::default();
        if let Some(path) = explicit.map(PathBuf::from).or_else(find_config_file) {
            if let Some(parsed) = parse_file(&path) {
                cfg = parsed;
            }
        }
        cfg.apply_gitconfig();
        // The empty theme means "unset"; normalize it to the default (`ansi`)
        // so theme-name checks (syntax_enabled, built-in lookup) are simple.
        if cfg.theme.is_empty() {
            cfg.theme = "ansi".into();
        }
        cfg
    }

    fn apply_gitconfig(&mut self) {
        let Ok(out) = Command::new("git")
            .args(["config", "--get-regexp", "^drift\\."])
            .output()
        else {
            return;
        };
        if !out.status.success() {
            return;
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some((key, val)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let val = val.trim();
            let key = key.trim_start_matches("drift.").to_ascii_lowercase();
            // Colors and styles live in git subsections: `[drift "colors"]` and
            // `[drift "styles"]`. Keys are kebab-case throughout (e.g.
            // `add-emph`), matching the config-file and palette token names.
            if let Some(field) = key.strip_prefix("colors.") {
                self.set_color(field, val);
            } else if let Some(name) = key.strip_prefix("styles.") {
                self.styles.insert(name.to_string(), val.to_string());
            } else {
                match key.as_str() {
                    "theme" => self.theme = val.to_string(),
                    "syntax" => self.syntax = parse_bool(val, self.syntax),
                    "intraline" => self.intraline = parse_bool(val, self.intraline),
                    "line-numbers" => self.line_numbers = parse_bool(val, self.line_numbers),
                    "tab-width" => self.tab_width = val.parse().unwrap_or(self.tab_width),
                    "sidebar" => self.sidebar = val.to_string(),
                    "sidebar-width" => {
                        self.sidebar_width = val.parse().unwrap_or(self.sidebar_width)
                    }
                    "sidebar-side" => self.sidebar_side = val.to_string(),
                    "editor" => self.editor = val.to_string(),
                    _ => {}
                }
            }
        }
    }

    /// Set a single palette color by field name (matching the `[colors]` keys).
    fn set_color(&mut self, field: &str, val: &str) {
        let c = &mut self.colors;
        let slot = match field {
            "add" => &mut c.add,
            "remove" => &mut c.remove,
            "context" => &mut c.context,
            "header" => &mut c.header,
            "line-number" => &mut c.line_number,
            "primary" => &mut c.primary,
            "secondary" => &mut c.secondary,
            "foreground" => &mut c.foreground,
            "background" => &mut c.background,
            "muted" => &mut c.muted,
            "surface" => &mut c.surface,
            "cursor" => &mut c.cursor,
            "add-emph" => &mut c.add_emph,
            "remove-emph" => &mut c.remove_emph,
            "add-line" => &mut c.add_line,
            "remove-line" => &mut c.remove_line,
            "header-line" => &mut c.header_line,
            _ => return,
        };
        *slot = val.to_string();
    }

    /// Resolve the editor command: config, then $VISUAL, $EDITOR, then vi.
    pub fn editor_cmd(&self) -> String {
        if !self.editor.is_empty() {
            return self.editor.clone();
        }
        std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into())
    }

    /// Whether syntax highlighting is on. The `ansi` theme stays plain: with
    /// only 16 terminal colors, layering highlight hues over the diff colors
    /// reads worse than leaving code alone.
    pub fn syntax_enabled(&self) -> bool {
        self.syntax && self.theme != "ansi"
    }

    /// Whether intra-line (word-level) emphasis is on. Off for `ansi` for the
    /// same reason as [`Self::syntax_enabled`].
    pub fn intraline_enabled(&self) -> bool {
        self.intraline && self.theme != "ansi"
    }
}

fn find_config_file() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(x).join("drift"));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".config/drift"));
    }
    for dir in &dirs {
        for name in [
            "config.toml",
            "config.yaml",
            "config.yml",
            "config.json",
        ] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    // Also honor a `~/.drift.toml` dotfile.
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".drift.toml");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn parse_file(path: &Path) -> Option<Config> {
    let text = std::fs::read_to_string(path).ok()?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let result = match ext {
        "json" => serde_json::from_str(&text).map_err(|e| e.to_string()),
        "yaml" | "yml" => serde_yaml::from_str(&text).map_err(|e| e.to_string()),
        _ => toml::from_str(&text).map_err(|e| e.to_string()),
    };
    match result {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("drift: ignoring bad config {}: {e}", path.display());
            None
        }
    }
}

fn parse_bool(v: &str, default: bool) -> bool {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => default,
    }
}

/// A built-in color palette: the named hues both the UI palette and the
/// syntax theme are derived from.
pub struct Builtin {
    pub bg: &'static str,
    pub fg: &'static str,
    pub red: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub orange: &'static str,
    pub blue: &'static str,
    pub purple: &'static str,
    pub cyan: &'static str,
    pub grey: &'static str,
    pub surface: &'static str,
    pub cursor: &'static str,
    pub add_line: &'static str,
    pub remove_line: &'static str,
    pub header_line: &'static str,
    pub add_emph: &'static str,
    pub remove_emph: &'static str,
}

/// Look up a named built-in palette. Returns None for unknown names (a syntect
/// theme name, say), in which case callers fall back to the `ansi` palette.
/// `ansi` maps to the terminal's own 16 colors so it inherits whatever palette
/// the user's terminal defines.
pub fn builtin_named(name: &str) -> Option<Builtin> {
    match name {
        "onedark" => Some(Builtin {
            bg: "#282c34",
            fg: "#abb2bf",
            red: "#e06c75",
            green: "#98c379",
            yellow: "#e5c07b",
            orange: "#d19a66",
            blue: "#61afef",
            purple: "#c678dd",
            cyan: "#56b6c2",
            grey: "#5c6370",
            surface: "#3b4048",
            cursor: "#3e4451",
            add_line: "#2b3a2e",
            remove_line: "#3f2d30",
            header_line: "#2c3543",
            add_emph: "#3d5943",
            remove_emph: "#6d3b40",
        }),
        "onelight" => Some(Builtin {
            bg: "#fafafa",
            fg: "#383a42",
            red: "#e45649",
            green: "#50a14f",
            yellow: "#c18401",
            orange: "#986801",
            blue: "#4078f2",
            purple: "#a626a4",
            cyan: "#0184bc",
            grey: "#a0a1a7",
            surface: "#eaeaeb",
            cursor: "#cdd1d8",
            add_line: "#e6f3e6",
            remove_line: "#fbe9e8",
            header_line: "#e3ebfb",
            add_emph: "#cdead0",
            remove_emph: "#f7d3d0",
        }),
        "ansi" => Some(Builtin {
            bg: "default",
            fg: "default",
            red: "red",
            green: "green",
            yellow: "yellow",
            orange: "brightred",
            blue: "blue",
            purple: "magenta",
            cyan: "cyan",
            grey: "brightblack",
            surface: "brightblack",
            cursor: "brightblack",
            add_line: "default",
            remove_line: "default",
            header_line: "brightblack",
            add_emph: "green",
            remove_emph: "red",
        }),
        "dracula" => Some(Builtin {
            bg: "#282a36",
            fg: "#f8f8f2",
            red: "#ff5555",
            green: "#50fa7b",
            yellow: "#f1fa8c",
            orange: "#ffb86c",
            blue: "#8be9fd",
            purple: "#bd93f9",
            cyan: "#8be9fd",
            grey: "#6272a4",
            surface: "#44475a",
            cursor: "#4d5066",
            add_line: "#2e4b41",
            remove_line: "#4a313b",
            header_line: "#2f3a4c",
            add_emph: "#36714d",
            remove_emph: "#713941",
        }),
        "gruvbox-dark" => Some(Builtin {
            bg: "#282828",
            fg: "#ebdbb2",
            red: "#fb4934",
            green: "#b8bb26",
            yellow: "#fabd2f",
            orange: "#fe8019",
            blue: "#83a598",
            purple: "#d3869b",
            cyan: "#8ec07c",
            grey: "#928374",
            surface: "#3c3836",
            cursor: "#504945",
            add_line: "#3f4028",
            remove_line: "#4a2d2a",
            header_line: "#2f3a3a",
            add_emph: "#595a27",
            remove_emph: "#70332c",
        }),
        "gruvbox-light" => Some(Builtin {
            bg: "#fbf1c7",
            fg: "#3c3836",
            red: "#9d0006",
            green: "#79740e",
            yellow: "#b57614",
            orange: "#af3a03",
            blue: "#076678",
            purple: "#8f3f71",
            cyan: "#427b58",
            grey: "#928374",
            surface: "#ebdbb2",
            cursor: "#d5c4a1",
            add_line: "#ded69e",
            remove_line: "#e6bc9d",
            header_line: "#d3e0dc",
            add_emph: "#c4bc79",
            remove_emph: "#d48c76",
        }),
        "nord" => Some(Builtin {
            bg: "#2e3440",
            fg: "#d8dee9",
            red: "#bf616a",
            green: "#a3be8c",
            yellow: "#ebcb8b",
            orange: "#d08770",
            blue: "#81a1c1",
            purple: "#b48ead",
            cyan: "#88c0d0",
            grey: "#4c566a",
            surface: "#3b4252",
            cursor: "#434c5e",
            add_line: "#414a4c",
            remove_line: "#453b47",
            header_line: "#333f50",
            add_emph: "#56635a",
            remove_emph: "#5f434e",
        }),
        "solarized-dark" => Some(Builtin {
            bg: "#002b36",
            fg: "#839496",
            red: "#dc322f",
            green: "#859900",
            yellow: "#b58900",
            orange: "#cb4b16",
            blue: "#268bd2",
            purple: "#6c71c4",
            cyan: "#2aa198",
            grey: "#586e75",
            surface: "#073642",
            cursor: "#094e5e",
            add_line: "#153d2d",
            remove_line: "#3d2229",
            header_line: "#0a3a4a",
            add_emph: "#2d5024",
            remove_emph: "#4b2d34",
        }),
        "solarized-light" => Some(Builtin {
            bg: "#fdf6e3",
            fg: "#657b83",
            red: "#dc322f",
            green: "#859900",
            yellow: "#b58900",
            orange: "#cb4b16",
            blue: "#268bd2",
            purple: "#6c71c4",
            cyan: "#2aa198",
            grey: "#93a1a1",
            surface: "#eee8d5",
            cursor: "#ddd6c1",
            add_line: "#e3e2b1",
            remove_line: "#f6cbbb",
            header_line: "#dae6ec",
            add_emph: "#cbcf84",
            remove_emph: "#efa497",
        }),
        "catppuccin-mocha" => Some(Builtin {
            bg: "#1e1e2e",
            fg: "#cdd6f4",
            red: "#f38ba8",
            green: "#a6e3a1",
            yellow: "#f9e2af",
            orange: "#fab387",
            blue: "#89b4fa",
            purple: "#cba6f7",
            cyan: "#94e2d5",
            grey: "#6c7086",
            surface: "#313244",
            cursor: "#45475a",
            add_line: "#343e40",
            remove_line: "#402f42",
            header_line: "#2a3048",
            add_emph: "#4c6155",
            remove_emph: "#664357",
        }),
        "catppuccin-latte" => Some(Builtin {
            bg: "#eff1f5",
            fg: "#4c4f69",
            red: "#d20f39",
            green: "#40a02b",
            yellow: "#df8e1d",
            orange: "#fe640b",
            blue: "#1e66f5",
            purple: "#8839ef",
            cyan: "#179299",
            grey: "#9ca0b0",
            surface: "#ccd0da",
            cursor: "#bcc0cc",
            add_line: "#c8dfc9",
            remove_line: "#e9bfcc",
            header_line: "#d5def6",
            add_emph: "#a6cfa0",
            remove_emph: "#e392a6",
        }),
        "tokyonight" => Some(Builtin {
            bg: "#1a1b26",
            fg: "#c0caf5",
            red: "#f7768e",
            green: "#9ece6a",
            yellow: "#e0af68",
            orange: "#ff9e64",
            blue: "#7aa2f7",
            purple: "#bb9af7",
            cyan: "#7dcfff",
            grey: "#565f89",
            surface: "#24283b",
            cursor: "#292e42",
            add_line: "#2f3831",
            remove_line: "#3d2a37",
            header_line: "#222c48",
            add_emph: "#47583d",
            remove_emph: "#653a49",
        }),
        "monokai" => Some(Builtin {
            bg: "#272822",
            fg: "#f8f8f2",
            red: "#f92672",
            green: "#a6e22e",
            yellow: "#e6db74",
            orange: "#fd971f",
            blue: "#66d9ef",
            purple: "#ae81ff",
            cyan: "#66d9ef",
            grey: "#75715e",
            surface: "#3e3d32",
            cursor: "#49483e",
            add_line: "#3b4624",
            remove_line: "#49282f",
            header_line: "#2c3b40",
            add_emph: "#526726",
            remove_emph: "#6e273d",
        }),
        _ => None,
    }
}

/// Default component styles, the built-in counterpart to [`builtin_named`]'s
/// colors. Each spec is `fg bg attrs...` resolved against the theme's palette,
/// so the specs are shared across themes and adapt through the palette — a
/// theme only appears here when it needs a genuinely different treatment.
///
/// `ansi` rides the terminal's own colors, and its `muted` (brightblack) is too
/// dim for the help-grid descriptions, so it uses the plain terminal foreground
/// there instead.
pub fn builtin_style(theme: &str, component: &str) -> &'static str {
    if theme == "ansi" && component == "help-desc" {
        return "default";
    }
    match component {
        "add" => "add",
        "remove" => "remove",
        "context" => "context",
        "header" => "header bold",
        "line-number" => "line-number",
        "statusbar" => "foreground surface",
        "statusbar-logo" => "background primary bold",
        "statusbar-filename" => "foreground surface bold",
        "statusbar-add" => "add surface",
        "statusbar-remove" => "remove surface",
        "statusbar-flags" => "muted surface",
        "statusbar-stats" => "foreground surface",
        "statusbar-search" => "secondary surface",
        "statusbar-watch" => "background add bold",
        "statusbar-help" => "background secondary bold",
        "help-key" => "muted bold",
        "help-desc" => "muted faint",
        "dialog" => "foreground background",
        "dialog-border" => "surface",
        "sidebar-border" => "surface",
        "search-match" => "background secondary bold",
        "search-current" => "background primary bold",
        _ => "",
    }
}


/// uncurses [`Color`]. Returns None for "default"/"none"/unrecognized.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("default") || s.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    if let Ok(idx) = s.parse::<u8>() {
        return Some(Color::Indexed(idx));
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "brightblack" | "gray" | "grey" => Color::BrightBlack,
        "brightred" => Color::BrightRed,
        "brightgreen" => Color::BrightGreen,
        "brightyellow" => Color::BrightYellow,
        "brightblue" => Color::BrightBlue,
        "brightmagenta" => Color::BrightMagenta,
        "brightcyan" => Color::BrightCyan,
        "brightwhite" => Color::BrightWhite,
        _ => return None,
    })
}

/// Resolved color palette: named tones from `[colors]` that style specs and
/// the renderer reference by name (e.g. `foreground`, `surface`, `add`).
pub struct Palette {
    map: HashMap<&'static str, Option<Color>>,
}

impl Palette {
    /// Build the palette for `theme`: each token is the user's color when set,
    /// otherwise the theme's built-in default (falling back to the `ansi`
    /// palette for unknown/syntect theme names, which don't repaint the chrome).
    pub fn new(c: &Colors, theme: &str) -> Self {
        let b = builtin_named(theme)
            .unwrap_or_else(|| builtin_named("ansi").expect("ansi palette exists"));
        // User value if set, else the theme's built-in default.
        let pick = |user: &str, default: &str| {
            parse_color(if user.is_empty() { default } else { user })
        };
        let map = HashMap::from([
            ("add", pick(&c.add, b.green)),
            ("remove", pick(&c.remove, b.red)),
            ("context", pick(&c.context, b.fg)),
            ("header", pick(&c.header, b.blue)),
            ("line-number", pick(&c.line_number, b.grey)),
            ("primary", pick(&c.primary, b.purple)),
            ("secondary", pick(&c.secondary, b.blue)),
            ("foreground", pick(&c.foreground, b.fg)),
            ("background", pick(&c.background, b.bg)),
            ("muted", pick(&c.muted, b.grey)),
            ("surface", pick(&c.surface, b.surface)),
            ("cursor", pick(&c.cursor, b.cursor)),
            ("add-line", pick(&c.add_line, b.add_line)),
            ("remove-line", pick(&c.remove_line, b.remove_line)),
            ("header-line", pick(&c.header_line, b.header_line)),
            ("add-emph", pick(&c.add_emph, b.add_emph)),
            ("remove-emph", pick(&c.remove_emph, b.remove_emph)),
        ]);
        Palette { map }
    }

    /// Resolve a token to a color: a palette name, else a literal color value
    /// (hex, index, or ANSI name). `default`/`none`/`-` all mean "no color",
    /// i.e. the terminal's own default fg/bg.
    pub fn color(&self, token: &str) -> Option<Color> {
        if matches!(token, "-" | "none" | "default") {
            return None;
        }
        match self.map.get(token) {
            Some(c) => *c,
            None => parse_color(token),
        }
    }
}

/// Parse a `fg bg attr...` style spec against the palette. The first two
/// color tokens are foreground then background; remaining known keywords set
/// attributes (`bold`, `faint`, `italic`, `underline`, `strikethrough`,
/// `blink`, `reverse`, `conceal`). Use `-`/`none`/`default` to skip a color
/// slot.
pub fn parse_style(spec: &str, palette: &Palette) -> Style {
    let mut style = Style::default();
    let mut slot = 0u8;
    for tok in spec.split_whitespace() {
        match tok.to_ascii_lowercase().as_str() {
            "bold" => style = style.bold(),
            "faint" | "dim" => style = style.faint(),
            "italic" => style = style.italic(),
            "underline" => style = style.underline(),
            "strikethrough" => style = style.strikethrough(),
            "blink" => style = style.blink(),
            "reverse" => style = style.reverse(),
            "conceal" | "hidden" => style = style.conceal(),
            _ => {
                let c = palette.color(tok);
                style = if slot == 0 { style.fg(c) } else { style.bg(c) };
                slot += 1;
            }
        }
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse() {
        assert!(matches!(parse_color("#ff8800"), Some(Color::Rgb(255, 136, 0))));
        assert!(matches!(parse_color("52"), Some(Color::Indexed(52))));
        assert!(matches!(parse_color("green"), Some(Color::Green)));
        assert!(parse_color("default").is_none());
    }

    #[test]
    fn toml_roundtrip() {
        let c: Config = toml::from_str("theme = \"x\"\nsyntax = false\n[colors]\nadd = \"blue\"\n").unwrap();
        assert_eq!(c.theme, "x");
        assert!(!c.syntax);
        assert_eq!(c.colors.add, "blue");
        // untouched fields keep defaults
        assert!(c.line_numbers);
    }

    #[test]
    fn builtin_style_is_theme_aware() {
        // Shared default for all themes...
        assert_eq!(builtin_style("onedark", "help-desc"), "muted faint");
        assert_eq!(builtin_style("dracula", "help-desc"), "muted faint");
        // ...except ansi, whose muted is too dim for help text.
        assert_eq!(builtin_style("ansi", "help-desc"), "default");
        // Non-help styles are shared even under ansi.
        assert_eq!(builtin_style("ansi", "help-key"), "muted bold");
    }

    #[test]
    fn palette_falls_back_to_theme_defaults() {
        // Unset color → the theme's built-in value; set color → the user's.
        let cols = Colors { add: "#123456".into(), ..Colors::default() };
        let pal = Palette::new(&cols, "onedark");
        assert!(matches!(pal.color("add"), Some(Color::Rgb(0x12, 0x34, 0x56))));
        assert_eq!(pal.color("remove"), parse_color("#e06c75")); // onedark red
    }

    #[test]
    fn style_spec_parses() {
        let cols = Colors {
            foreground: "white".into(),
            surface: "brightblack".into(),
            primary: "cyan".into(),
            ..Colors::default()
        };
        let pal = Palette::new(&cols, "onedark");
        let s = parse_style("foreground surface bold", &pal);
        assert_eq!(s.fg, Some(Color::White));
        assert_eq!(s.bg, Some(Color::BrightBlack));
        assert!(!s.attrs.is_empty()); // bold set
        // `-` skips the fg slot; bg still applies.
        let s2 = parse_style("- primary", &pal);
        assert_eq!(s2.fg, None);
        assert_eq!(s2.bg, Some(Color::Cyan));
        // `default`/`none` request the terminal's own color for either slot.
        let s3 = parse_style("default primary", &pal);
        assert_eq!(s3.fg, None);
        assert_eq!(s3.bg, Some(Color::Cyan));
        let s4 = parse_style("foreground none", &pal);
        assert_eq!(s4.fg, Some(Color::White));
        assert_eq!(s4.bg, None);
    }
}
