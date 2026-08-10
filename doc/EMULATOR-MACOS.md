# Running the emulator on macOS (platokin fork, phase 0)

Upstream's `doc/BUILD.md` documents the Linux path only ("install MuPDF 1.27.0,
DjVuLibre, FreeType, HarfBuzz" and then `./run-emulator.sh`). This is the macOS
equivalent, driven entirely by `xbuild.py`.

## One-time

```sh
brew install sdl2 freetype harfbuzz jpeg-turbo openjpeg jbig2dec gumbo-parser
```

DjVuLibre is deliberately **not** in that list — DjVu is behind a cargo feature
that is off in this fork (see `PATCHES.md`).

## Build and run

```sh
python3 xbuild.py host          # build MuPDF 1.27.0 + the wrapper + the emulator
python3 xbuild.py host --run    # ... and launch it
```

The first run takes a few minutes (MuPDF's `make generate` bakes the built-in
fonts and CMaps into C). Afterwards it is incremental: `.xbuild/mupdf` carries a
`.xbuild-built` stamp, so only cargo re-runs.

`xbuild.py` also unpacks `hyphenation-patterns/` out of upstream's sha256-pinned
release zip. They are not in the git tree, and Plato does not hyphenate without
them — which matters, because judging the typography is the whole point of
phase 0.

## Settings.toml

The emulator reads `Settings.toml` **from the current working directory**
(`plato_core::settings::SETTINGS_PATH` is the bare relative name), and it looks
for `fonts/`, `css/`, `keyboard-layouts/` and `hyphenation-patterns/` there too.
So always run it from the repo root. A minimal file:

```toml
selected-library = 0

[[libraries]]
name = "Test"
path = "test-library"
mode = "database"
```

`mode = "database"` makes Plato keep a `.metadata.json` next to the books
(rather than filesystem mode). Books are indexed on first launch; the console
prints `Add new entry: …` per book.

`Settings.toml`, `test-library/` and `hyphenation-patterns/` are all gitignored.

## Emulator controls

The window is 600×800 by default — Plato claims to be a "Kobo Touch A/B". It is
a touch-only UI, so:

| Input | Meaning |
|---|---|
| Left mouse button | a finger: click = tap, drag = swipe |

In the reader view the page is divided into nine regions (`geom::Region::from_point`,
with `strip_width = 0.6` and `corner_width = 0.4` from `settings::ReaderSettings`):

| Tap where | Action |
|---|---|
| Centre | toggle the top/bottom bars |
| Left edge strip | previous page (`west_strip`) |
| Right edge strip | next page (`east_strip`) |
| Top edge strip | toggle the bars |
| Bottom edge strip | toggle the bars (`south_strip`; can be set to next page) |
| Top-left corner | go to the last page visited |
| Top-right corner | toggle bookmark |
| Bottom-left corner | table of contents |
| Bottom-right corner | go-to-page dialog (`south_east_corner`) |

Swiping left/right also turns pages. Keys:

| Key | Action |
|---|---|
| `Esc` | back (and, at the top level, quit) |
| `[` / `]` | rotate the screen |
| `S` | save `screenshot-YYYYmmdd_HHMMSS.png` in the CWD |
| `B` `F` `P` `L` `H` `E` `G` | the Kobo hardware buttons |

`Ctrl`-modified keys are mapped separately in `crates/emulator/src/main.rs` —
read `code_from_key` there for the full list.

## Note on screenshots from an automated session

`screencapture` needs the Screen Recording TCC permission and `osascript`
keystroke injection needs Accessibility; neither is granted to a headless agent
shell, so an agent cannot photograph the SDL window or press `S` for you. To
verify rendering without the GUI, link a tiny binary against `plato-core` and
call `EpubDocument::pixmap(...)` + `Pixmap::save(...)` — that exercises exactly
the same layout engine the reader view uses.
