//! Configuration: theme, colors, and settings loaded from a TOML/YAML/JSON
//! file and overlaid with any `[diffv]` values from git config.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use uncurses::color::Color;
use uncurses::style::Style;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// Editor command; falls back to $EDITOR / $VISUAL when empty.
    pub editor: String,
    pub colors: Colors,
    /// Component styles, each a `fg bg attrs...` spec resolved against the
    /// color palette. Overrides the built-in defaults per named component.
    #[serde(default)]
    pub styles: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub add: String,
    pub remove: String,
    pub context: String,
    pub header: String,
    pub line_number: String,
    /// Primary accent: the "diffv" badge background in the footer.
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
            add_emph: String::new(),
            remove_emph: String::new(),
        }
    }
}

impl Config {
    /// Load config: defaults, then a config file (explicit path or the first
    /// found in standard locations), then git config `[diffv]` overrides.
    pub fn load(explicit: Option<&Path>) -> Self {
        let mut cfg = Config::default();
        if let Some(path) = explicit.map(PathBuf::from).or_else(find_config_file) {
            if let Some(parsed) = parse_file(&path) {
                cfg = parsed;
            }
        }
        cfg.apply_gitconfig();
        cfg.resolve_theme_defaults();
        cfg
    }

    /// Fill any unset theme/color with the built-in onedark or onelight
    /// palette, chosen by the terminal background.
    fn resolve_theme_defaults(&mut self) {
        let dark = prefer_dark();
        let b = builtin(dark);
        if self.theme.is_empty() {
            self.theme = if dark { "onedark" } else { "onelight" }.into();
        }
        let c = &mut self.colors;
        let fill = |s: &mut String, v: &str| {
            if s.is_empty() {
                *s = v.to_string();
            }
        };
        fill(&mut c.add, b.green);
        fill(&mut c.remove, b.red);
        fill(&mut c.context, b.fg);
        fill(&mut c.header, b.blue);
        fill(&mut c.line_number, b.grey);
        fill(&mut c.primary, b.purple);
        fill(&mut c.secondary, b.blue);
        fill(&mut c.foreground, b.fg);
        fill(&mut c.background, b.bg);
        fill(&mut c.muted, b.grey);
        fill(&mut c.surface, b.surface);
        fill(&mut c.cursor, b.cursor);
        fill(&mut c.add_line, b.add_line);
        fill(&mut c.remove_line, b.remove_line);
        fill(&mut c.add_emph, b.add_emph);
        fill(&mut c.remove_emph, b.remove_emph);
    }

    fn apply_gitconfig(&mut self) {
        let Ok(out) = Command::new("git")
            .args(["config", "--get-regexp", "^diffv\\."])
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
            match key.trim_start_matches("diffv.").to_ascii_lowercase().as_str() {
                "theme" => self.theme = val.to_string(),
                "syntax" => self.syntax = parse_bool(val, self.syntax),
                "intraline" => self.intraline = parse_bool(val, self.intraline),
                "linenumbers" => self.line_numbers = parse_bool(val, self.line_numbers),
                "tabwidth" => self.tab_width = val.parse().unwrap_or(self.tab_width),
                "editor" => self.editor = val.to_string(),
                "coloradd" => self.colors.add = val.to_string(),
                "colorremove" => self.colors.remove = val.to_string(),
                "colorcontext" => self.colors.context = val.to_string(),
                "colorheader" => self.colors.header = val.to_string(),
                "colorlinenumber" => self.colors.line_number = val.to_string(),
                "colorprimary" => self.colors.primary = val.to_string(),
                "colorsecondary" => self.colors.secondary = val.to_string(),
                "colorforeground" => self.colors.foreground = val.to_string(),
                "colorbackground" => self.colors.background = val.to_string(),
                "colormuted" => self.colors.muted = val.to_string(),
                "colorsurface" => self.colors.surface = val.to_string(),
                "colorcursor" => self.colors.cursor = val.to_string(),
                "coloraddemph" => self.colors.add_emph = val.to_string(),
                "colorremoveemph" => self.colors.remove_emph = val.to_string(),
                "coloraddline" => self.colors.add_line = val.to_string(),
                "colorremoveline" => self.colors.remove_line = val.to_string(),
                other => {
                    // diffv.style<name> overrides a component style spec.
                    if let Some(name) = other.strip_prefix("style") {
                        if !name.is_empty() {
                            self.styles.insert(name.to_string(), val.to_string());
                        }
                    }
                }
            }
        }
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
}

fn find_config_file() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(x).join("diffv"));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".config/diffv"));
        dirs.push(PathBuf::from(home));
    }
    for dir in dirs {
        for name in [
            "config.toml",
            "config.yaml",
            "config.yml",
            "config.json",
            ".diffv.toml",
        ] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
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
            eprintln!("diffv: ignoring bad config {}: {e}", path.display());
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

/// Whether to prefer the dark palette. Uses the `COLORFGBG` hint (background
/// index in the last field); defaults to dark when unknown.
pub fn prefer_dark() -> bool {
    // ponytail: COLORFGBG heuristic only; add an OSC 11 query if this misreads
    // some terminals.
    if let Ok(v) = std::env::var("COLORFGBG") {
        if let Some(bg) = v.rsplit(';').next() {
            if let Ok(n) = bg.trim().parse::<u8>() {
                return matches!(n, 0..=6 | 8);
            }
        }
    }
    true
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
    pub add_emph: &'static str,
    pub remove_emph: &'static str,
}

/// The onedark (dark) or onelight (light) palette.
pub fn builtin(dark: bool) -> Builtin {
    if dark {
        Builtin {
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
            add_emph: "#3d5943",
            remove_emph: "#6d3b40",
        }
    } else {
        Builtin {
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
            add_emph: "#cdead0",
            remove_emph: "#f7d3d0",
        }
    }
}

/// Parse a color name, `#rrggbb` hex, or 0-255 palette index into a
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
    pub fn new(c: &Colors) -> Self {
        let map = HashMap::from([
            ("add", parse_color(&c.add)),
            ("remove", parse_color(&c.remove)),
            ("context", parse_color(&c.context)),
            ("header", parse_color(&c.header)),
            ("line_number", parse_color(&c.line_number)),
            ("primary", parse_color(&c.primary)),
            ("secondary", parse_color(&c.secondary)),
            ("foreground", parse_color(&c.foreground)),
            ("background", parse_color(&c.background)),
            ("muted", parse_color(&c.muted)),
            ("surface", parse_color(&c.surface)),
            ("cursor", parse_color(&c.cursor)),
            ("add_line", parse_color(&c.add_line)),
            ("remove_line", parse_color(&c.remove_line)),
            ("add_emph", parse_color(&c.add_emph)),
            ("remove_emph", parse_color(&c.remove_emph)),
        ]);
        Palette { map }
    }

    /// Resolve a token to a color: a palette name, else a literal color value
    /// (hex, index, or ANSI name). `none`/`default`/`-` mean "no color".
    pub fn color(&self, token: &str) -> Option<Color> {
        if token == "-" {
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
    fn style_spec_parses() {
        let cols = Colors {
            foreground: "white".into(),
            surface: "brightblack".into(),
            primary: "cyan".into(),
            ..Colors::default()
        };
        let pal = Palette::new(&cols);
        let s = parse_style("foreground surface bold", &pal);
        assert_eq!(s.fg, Some(Color::White));
        assert_eq!(s.bg, Some(Color::BrightBlack));
        assert!(!s.attrs.is_empty()); // bold set
        // `-` skips the fg slot; bg still applies.
        let s2 = parse_style("- primary", &pal);
        assert_eq!(s2.fg, None);
        assert_eq!(s2.bg, Some(Color::Cyan));
    }
}
