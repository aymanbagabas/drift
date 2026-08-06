# Configuring drift

drift runs with zero config. Everything below is optional — override only what
you want.

## Where config comes from

Settings are merged in this order, each layer overriding the one before:

1. Built-in defaults.
2. A config file — the first that exists, or the one passed with `--config`:
   - `$XDG_CONFIG_HOME/drift/config.{toml,yaml,yml,json}`
   - `~/.config/drift/config.{toml,yaml,yml,json}`
   - `~/.drift.toml`
3. Git config `[drift]` keys (great for per-repo overrides via a repo-local
   `.git/config`).

A config file can be TOML, YAML, or JSON. Unknown keys are rejected, so a typo
surfaces instead of silently doing nothing.

## Settings

| Key | Type | Default | What it does |
|-----|------|---------|--------------|
| `theme` | string | `ansi` | Palette name (see [Themes](#themes)). Unknown names fall back to `ansi`. |
| `syntax` | bool | `true` | Syntax-highlight diff content. Always off under the `ansi` theme. |
| `intraline` | bool | `true` | Word-level change emphasis. Always off under `ansi`. |
| `line-numbers` | bool | `true` | Show old/new line-number gutter. |
| `tab-width` | int | `4` | Spaces per tab when rendering. |
| `sidebar` | string | `auto` | File-list sidebar: `auto` (open when terminal ≥ 150 wide), `always`, or `never`. The `b` key overrides at runtime. |
| `sidebar-width` | int | `30` | Sidebar width in cells (including divider). Drag the divider to override at runtime. |
| `sidebar-side` | string | `left` | Which side the sidebar sits on: `left` or `right`. |
| `editor` | string | *(empty)* | Editor command for the `e` key. Empty falls back to `$VISUAL`, then `$EDITOR`, then `vi`. |

### TOML example

```toml
theme = "dracula"
syntax = true
intraline = true
line-numbers = true
tab-width = 4
sidebar = "auto"
sidebar-width = 30
sidebar-side = "left"
```

### Git config example

Keys are kebab-case, under the `[drift]` section:

```sh
git config drift.theme nord
git config drift.line-numbers false
git config drift.sidebar-side right
```

## Colors

The palette lives under `[colors]` (or git subsection `[drift "colors"]`). Any
value left unset is filled from the chosen theme. Each value accepts a literal
`#rrggbb`, a 0–255 index, an ANSI name (e.g. `blue`), or `default`/`none`/`-`
for the terminal's own default.

| Key | What it colors |
|-----|----------------|
| `add` | Added lines |
| `remove` | Removed lines |
| `context` | Unchanged context lines |
| `header` | Hunk headers |
| `line-number` | Line-number gutter |
| `primary` | Primary accent: the `drift` logo badge background |
| `secondary` | Secondary accent: file names, the `? help` badge, dialog backgrounds |
| `foreground` | Bright body text |
| `background` | Text drawn on accent badges / dialog surfaces |
| `muted` | Dim tone: flags, help descriptions |
| `surface` | Status bar and chip background |
| `cursor` | Current-line highlight background |
| `add-line` | Whole-line wash behind added lines |
| `remove-line` | Whole-line wash behind removed lines |
| `header-line` | Spanning band behind a hunk header |
| `add-emph` | Intra-line changed-word background (added) |
| `remove-emph` | Intra-line changed-word background (removed) |

```toml
[colors]
add    = "#50fa7b"
remove = "#ff5555"
cursor = "#4d5066"
```

## Component styles

`[styles]` (or `[drift "styles"]`) maps a named UI component to a
`fg bg attrs...` spec, resolved against the palette above. Only the components
you name are overridden.

**Attributes:** `bold`, `faint`/`dim`, `italic`, `underline`, `strikethrough`,
`blink`, `reverse`, `conceal`.

| Component | Default spec |
|-----------|--------------|
| `add` | `add` |
| `remove` | `remove` |
| `context` | `context` |
| `header` | `header bold` |
| `line-number` | `line-number` |
| `statusbar` | `foreground surface` |
| `statusbar-logo` | `background primary bold` |
| `statusbar-filename` | `foreground surface bold` |
| `statusbar-add` | `add surface` |
| `statusbar-remove` | `remove surface` |
| `statusbar-flags` | `muted surface` |
| `statusbar-stats` | `foreground surface` |
| `statusbar-search` | `secondary surface` |
| `statusbar-watch` | `background add bold` |
| `statusbar-help` | `background secondary bold` |
| `help-key` | `muted bold` |
| `help-desc` | `muted faint` |
| `dialog` | `foreground background` |
| `dialog-border` | `surface` |
| `sidebar-border` | `surface` |
| `search-match` | `background secondary bold` |
| `search-current` | `background primary bold` |

```toml
[styles]
header    = "header bold underline"
statusbar = "foreground surface"
help-desc = "muted italic"
```

## Themes

Built-in palettes: `ansi` (default — follows your terminal's 16 colors),
`onedark`, `onelight`, `dracula`, `gruvbox-dark`, `gruvbox-light`, `nord`,
`solarized-dark`, `solarized-light`, `catppuccin-mocha`, `catppuccin-latte`,
`tokyonight`, `monokai`.

The `themes/` directory ships each one as a ready-to-copy config file:

```sh
mkdir -p ~/.config/drift
cp themes/dracula.toml ~/.config/drift/config.toml
```

## Command-line flags

Per-run options (see `drift --help` for the full list):

| Flag | What it does |
|------|--------------|
| `--staged` (`--cached`) | Show staged changes (index vs HEAD) |
| `-A`, `--all` | Also show untracked files as new-file diffs |
| `-w`, `--watch` | Watch the index/refs and refresh on change |
| `--interval MS` | Working-tree poll interval in watch mode (default 300) |
| `-C`, `--directory DIR` | Run as if started in `DIR` (like `git -C`) |
| `-c`, `--config PATH` | Use a specific config file |
| `--no-syntax` | Disable syntax highlighting for this run |
| `--ignore-whitespace` | Ignore whitespace-only changes |
| `-U`, `--context N` | Lines of context around each change |
| `--diff-algorithm ALGO` | `myers`, `minimal`, `patience`, or `histogram` |
