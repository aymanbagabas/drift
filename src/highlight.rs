//! Syntax highlighting via syntect. Produces colored spans for diff content
//! lines, mapped to uncurses RGB colors.

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, Theme, ThemeItem, ThemeSet, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use uncurses::color::Color;

use crate::config::builtin;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    enabled: bool,
}

impl Highlighter {
    pub fn new(theme_name: &str, enabled: bool) -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let theme = match theme_name {
            "onedark" => builtin_theme(true),
            "onelight" => builtin_theme(false),
            other => ThemeSet::load_defaults()
                .themes
                .get(other)
                .cloned()
                .unwrap_or_else(|| builtin_theme(true)),
        };
        Highlighter {
            syntaxes,
            theme,
            enabled,
        }
    }

    /// Start a per-file highlighter keyed off the file path's extension.
    /// Lines must be fed in file order so multi-line constructs track state.
    pub fn file<'a>(&'a self, path: &str) -> FileHighlighter<'a> {
        let syntax = self.enabled.then(|| syntax_for(&self.syntaxes, path));
        FileHighlighter {
            inner: syntax.map(|s| HighlightLines::new(s, &self.theme)),
            syntaxes: &self.syntaxes,
        }
    }
}

fn syntax_for<'a>(ss: &'a SyntaxSet, path: &str) -> &'a SyntaxReference {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    ss.find_syntax_by_extension(ext)
        .or_else(|| {
            std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| ss.find_syntax_by_extension(n))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

pub struct FileHighlighter<'a> {
    inner: Option<HighlightLines<'a>>,
    syntaxes: &'a SyntaxSet,
}

impl FileHighlighter<'_> {
    /// Highlight one line, returning colored spans. When highlighting is off
    /// or fails, returns a single uncolored span covering the whole line.
    pub fn line(&mut self, text: &str) -> Vec<(Option<Color>, String)> {
        if let Some(h) = self.inner.as_mut() {
            if let Ok(ranges) = h.highlight_line(text, self.syntaxes) {
                return ranges
                    .into_iter()
                    .map(|(style, s)| (Some(conv(style.foreground)), s.to_string()))
                    .collect();
            }
        }
        vec![(None, text.to_string())]
    }
}

fn conv(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Parse `#rrggbb` into a syntect color (opaque).
fn hexc(h: &str) -> SynColor {
    let h = h.trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    SynColor {
        r: byte(0),
        g: byte(2),
        b: byte(4),
        a: 255,
    }
}

/// Build a syntect theme from the onedark/onelight palette, mapping the common
/// scopes to the palette hues so highlighting matches the UI colors.
fn builtin_theme(dark: bool) -> Theme {
    let b = builtin(dark);
    let item = |scope: &str, hex: &str, italic: bool| ThemeItem {
        scope: scope.parse().unwrap(),
        style: syntect::highlighting::StyleModifier {
            foreground: Some(hexc(hex)),
            background: None,
            font_style: italic.then_some(FontStyle::ITALIC),
        },
    };
    Theme {
        name: Some(if dark { "onedark" } else { "onelight" }.to_string()),
        author: None,
        settings: ThemeSettings {
            background: Some(hexc(b.bg)),
            foreground: Some(hexc(b.fg)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            item("comment", b.grey, true),
            item("string", b.green, false),
            item("constant.numeric", b.orange, false),
            item("constant.language, constant.character, constant.other", b.orange, false),
            item("support.constant", b.cyan, false),
            item("keyword, storage, keyword.control", b.purple, false),
            item("keyword.operator", b.cyan, false),
            item("entity.name.function, support.function, meta.function-call", b.blue, false),
            item(
                "entity.name.type, entity.name.class, support.type, support.class, storage.type",
                b.yellow,
                false,
            ),
            item("variable.parameter", b.red, false),
            item("entity.name.tag", b.red, false),
            item("entity.other.attribute-name", b.orange, false),
        ],
    }
}
