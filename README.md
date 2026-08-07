<p align="center">
  <img src="demo/mascot.svg" alt="drift" width="180">
</p>

<h1 align="center">drift</h1>

<p align="center">
  <a href="https://crates.io/crates/drift-diff"><img src="https://img.shields.io/crates/v/drift-diff.svg" alt="Crates.io"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
</p>

<p align="center">
A git diff pager that actually wants to be looked at.
</p>

<p align="center">
  <picture>
    <source srcset="https://github.com/aymanbagabas/drift/blob/main/demo/drift.gif?raw=true" type="image/gif">
    <source srcset="https://raw.githubusercontent.com/aymanbagabas/drift/main/demo/drift.gif" type="image/gif">
    <img width="900" alt="drift in action" src="demo/drift.gif">
  </picture>
</p>

`drift` takes the diff you already know and drops it into a real terminal UI:
syntax highlighting, word-level change emphasis, split or unified views, a
file-list modal and a live sidebar you can click and drag to resize,
jump-to-hunk, in-file search, open-in-`$EDITOR`, and a live mode that repaints
the moment your index or branch moves. Point it at a commit, your staging area,
or your working tree, or just pipe a diff into it like any other pager.

No config needed to start. Themes, colors, and keys are all yours to bend later.

## Install

### Homebrew (macOS, Linux)

```sh
brew install aymanbagabas/tap/drift
```

### Scoop (Windows)

```sh
scoop bucket add aymanbagabas https://github.com/aymanbagabas/scoop-bucket
scoop install drift
```

### Cargo

```sh
cargo install drift-diff
```

### npm

```sh
npm install -g @aymanbagabas/drift
```

### Debian / Ubuntu

```sh
echo 'deb [trusted=yes] https://repo.aymanbagabas.com/apt/ /' | sudo tee /etc/apt/sources.list.d/aymanbagabas.list
sudo apt update && sudo apt install drift
```

### Fedora / RHEL

```sh
echo '[aymanbagabas]
name=Ayman Bagabas
baseurl=https://repo.aymanbagabas.com/yum/
enabled=1
gpgcheck=0' | sudo tee /etc/yum.repos.d/aymanbagabas.repo
sudo yum install drift
```

### Arch Linux (AUR)

```sh
yay -S drift-diff-bin
```

### Prebuilt binaries

Grab an archive for your platform from the
[latest release](https://github.com/aymanbagabas/drift/releases/latest) and put
the `drift` binary on your `PATH`.

Every method installs a `drift` binary.

## Use it

```sh
drift                     # working tree vs HEAD
drift -A                  # ...and untracked files too
drift --staged            # what's staged (alias: --cached)
drift HEAD~3              # a single commit
drift main..feature       # a range
drift -w                  # watch the repo and refresh on every change (great alongside AI coding agents)
git diff | drift          # pager mode: read a diff from stdin
```

Make it your default git diff pager:

```sh
git config --global pager.diff drift
```

Some flags worth knowing:

| Flag | What it does |
|------|--------------|
| `-w`, `--watch` | Refresh when the git index or refs change |
| `-A`, `--all` | Also show untracked files (working-tree view only) |
| `-C`, `--directory DIR` | Run as if started in `DIR` (worktrees, bare repos) |
| `-c`, `--config FILE` | Use a specific config file |
| `--no-syntax` | Turn off syntax highlighting for this run |
| `-U`, `--context N` | Lines of context around each change |
| `--ignore-whitespace` | Ignore whitespace-only changes |
| `--diff-algorithm ALGO` | `myers`, `minimal`, `patience`, or `histogram` |
| `-- PATHSPEC...` | Limit to paths, e.g. `drift -- src/ docs/` |

## Keys

| Key | Action |
|-----|--------|
| `j` `k` / `↑` `↓` | Move the cursor |
| `h` `l` / `←` `→` | Scroll horizontally (for lines wider than the view) |
| `0` `$` | Scroll to line start / end |
| `d` `u` / `^d` `^u` | Half page down / up |
| `space` `f` `^f` / `b` `^b` | Full page down / up |
| `^e` `^y` | Scroll one line down / up |
| `g` `G` | Top / bottom |
| `H` `M` `L` | Cursor to screen top / middle / bottom |
| `{` `}` | Previous / next hunk |
| `[` `]` / `tab` `⇧tab` | Previous / next file |
| `/` | Search the current file (regex, smart-case) |
| `n` `N` | Next / previous match |
| `s` | Toggle split view |
| `F` | File list modal |
| `B` | Toggle the file sidebar |
| `w` | Toggle watch mode |
| `V` | Start / cancel a line selection; any motion key extends it |
| `y` | Copy the selection, or the cursor line when nothing is selected |
| `Y` | Copy the whole current file |
| `enter` | Expand folded context, or open the file at the cursor |
| `v` | Open the current file in `$EDITOR` |
| `r` | Refresh |
| `?` | Toggle the help footer |
| `q` | Quit |

## Mouse

drift is fully mouse-driven too:

- Click a file in the sidebar to jump straight to it in the diff.
- Click a file in the file modal to select it; the modal stays open until you
  click outside it (or press `enter`).
- Drag the sidebar's divider to resize it, live.
- In split view, drag the divider between the two panes to rebalance them.
- Click the `? help` badge in the status bar to toggle the help footer.
- Drag across the diff to select text; it lands on your system clipboard (over
  SSH too, via OSC 52). In split view the selection stays within one pane, so
  you copy just the old or just the new side.
- Scroll wheel moves the page through the diff (the cursor stays put until it
  reaches an edge). Scroll the wheel left/right to pan wide lines horizontally.

## Themes

Set `theme = "..."` in your config. Built in and ready:

- `onedark`, `onelight` (the defaults, picked by your terminal background)
- `dracula`
- `gruvbox-dark`, `gruvbox-light`
- `nord`
- `solarized-dark`, `solarized-light`
- `catppuccin-mocha`, `catppuccin-latte`
- `tokyonight`
- `monokai`
- `ansi`, which borrows your terminal's own 16 colors so drift matches
  whatever palette you already run. It leaves code text and word-diff
  emphasis alone, since 16 colors are too few to layer cleanly over diff
  colors.

Any [syntect](https://github.com/trishume/syntect) theme name works too (for
example `base16-ocean.dark`), it just won't repaint the UI chrome to match.

The `themes/` directory has a full, commented config for every built-in theme.
Copy one and go:

```sh
mkdir -p ~/.config/drift
cp themes/dracula.toml ~/.config/drift/config.toml
```

## Config

drift looks for a config file, in order, at:

- `$XDG_CONFIG_HOME/drift/config.{toml,yaml,yml,json}`
- `~/.config/drift/config.{toml,yaml,yml,json}`
- `~/.drift.toml`

TOML, YAML, and JSON all work. The top-level knobs:

```toml
theme         = "onedark"   # any built-in or syntect theme name
syntax        = true        # highlight diff content
intraline     = true        # word-level change emphasis
line-numbers  = true        # old/new line-number gutter
tab-width     = 4
editor        = ""          # falls back to $VISUAL, then $EDITOR, then vi
sidebar       = "auto"      # "auto" (opens at width >= 150), "always", or "never"
sidebar-width = 30          # sidebar columns; drag its divider to resize live
sidebar-side  = "left"      # "left" or "right"
```

Colors come in two layers. `[colors]` is a named palette; `[styles]` maps UI
components to a `fg bg attrs...` spec resolved against that palette. Every color
token accepts a palette name, a literal `#rrggbb`, a 0-255 index, an ANSI color
name, or `default`/`none`/`-` for the terminal's own color. See any file in
`themes/` for the full set with comments.

### From git config

Anything you can set in a config file, you can set in git config under the
`drift` section, which is handy for per-repo overrides. Colors and styles go in
`colors` and `styles` subsections. Git forbids `_` in a key, so a field like
`add_emph` becomes `add-emph`:

```sh
git config drift.theme nord
git config drift.line-numbers false
git config drift.colors.add '#00ff00'
git config drift.colors.add-emph '#003300'
git config drift.styles.statusbar 'foreground surface bold'
```

Or in `~/.gitconfig` directly:

```ini
[drift]
    theme = nord
    line-numbers = false
[drift "colors"]
    add = "#00ff00"
    add-emph = "#003300"
[drift "styles"]
    statusbar = "foreground surface bold"
```

For the full list of every setting, color, component style, and flag, see
[CONFIG.md](CONFIG.md).

## Built with

drift's terminal UI is powered by [uncurses](https://github.com/aymanbagabas/uncurses).

## License

MIT
