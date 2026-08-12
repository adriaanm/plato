![Logo](artworks/plato-logo.svg)

> ## This fork: Plato on the Kindle Paperwhite 3
>
> This is a fork of [baskerville/plato](https://github.com/baskerville/plato)
> focused on running Plato on the **Kindle Paperwhite 3** (`muscat`,
> i.MX6SL "Wario", firmware 5.16.2.1.1, kernel `3.0.35-lab126`, 1072×1448
> @ 300 dpi, armv7 **soft-float** ABI, glibc 2.20). What it adds, each piece
> recorded in [PATCHES.md](PATCHES.md):
>
> - A Kindle backend: the lab126 `mxcfb` ioctl dialect (72-byte
>   `mxcfb_update_data`, REAGL waveforms), `cyttsp4_mt` multi-touch
>   (`TouchProto::MultiSlot`), `max77696` battery and frontlight, selected via
>   `PLATO_DEVICE=kindle-pw3`.
> - A self-contained cross-build (`xbuild.py`) using `zig cc` +
>   `cargo-zigbuild` — no vendor toolchain, sha256-pinned third-party sources,
>   an ABI gate that rejects hard-float output.
> - PDF reading tuned for academic papers: automatic content-box cropping,
>   continuous scroll with an overlap anchor on page turns, and automatic
>   two-column detection with column-wise navigation.
> - Robustness fixes that are not Kindle-specific: a missing external helper
>   can no longer crash the app, and a refused suspend no longer loops.
> - Getting documents onto the device, host half included: `crates/foldersync`
>   (pull a folder from a Mac on the LAN), `crates/platonic` (push one document
>   and open it), `crates/pairing` (pairing from a code shown on the panel —
>   SPAKE2, same-WiFi only, no account and no cloud) and `crates/platonic-recv`,
>   which is what a paired Mac gets **instead of** a shell.
>
> ### The sibling repo
>
> This repository is the reader itself. **[adriaanm/platokin](https://github.com/adriaanm/platokin)**
> holds the tooling that makes it work on a Paperwhite 3 — installing and
> launching it, the host-side scripts, and the design docs explaining why things
> are the way they are here.
>
> The split keeps this repo a Plato fork that can be read, built and upstreamed
> on its own. Where a feature has two halves, both halves are here and the
> design doc is next door.
>
> **We're happy to upstream any of this if there's interest** — the build
> hygiene, the PDF features and the robustness fixes were written to be
> upstreamable; the Kindle backend is cleanly additive. Open an issue or PR
> conversation on this fork.

*Plato* is a document reader for *Kobo*'s e-readers.

Documentation: [GUIDE](doc/GUIDE.md), [MANUAL](doc/MANUAL.md) and [BUILD](doc/BUILD.md).

## Supported firmwares

Any 4.*X*.*Y* firmware, with *X* ≥ 6, will do.

## Supported devices

- *Libra Colour*.
- *Clara Colour*.
- *Clara BW*.
- *Elipsa 2E*.
- *Clara 2E*.
- *Libra 2*.
- *Sage*.
- *Elipsa*.
- *Nia*.
- *Libra H₂O*.
- *Forma*.
- *Clara HD*.
- *Aura H₂O Edition 2*.
- *Aura Edition 2*.
- *Aura ONE*.
- *Glo HD*.
- *Aura H₂O*.
- *Aura*.
- *Glo*.
- *Touch C*.
- *Touch B*.

## Supported formats

- PDF, CBZ, FB2, MOBI, XPS and TXT via [MuPDF](https://mupdf.com/index.html).
- ePUB through a built-in renderer.
- DJVU via [DjVuLibre](http://djvu.sourceforge.net/index.html).

## Features

- Crop the margins.
- Continuous fit-to-width zoom mode with line preserving cuts.
- Rotate the screen (portrait ↔ landscape).
- Adjust the contrast.
- Define words using *dictd* dictionaries.
- Annotations, highlights and bookmarks.
- Retrieve articles from online sources through [hooks](doc/HOOKS.md) (an example *wallabag* [article fetcher](doc/ARTICLE_FETCHER.md) is provided).

[![Tn01](artworks/thumbnail01.png)](artworks/screenshot01.png) [![Tn02](artworks/thumbnail02.png)](artworks/screenshot02.png) [![Tn03](artworks/thumbnail03.png)](artworks/screenshot03.png) [![Tn04](artworks/thumbnail04.png)](artworks/screenshot04.png)

## Donations

[![Donate](https://img.shields.io/badge/Donate-PayPal-green.svg)](https://www.paypal.com/cgi-bin/webscr?cmd=_s-xclick&hosted_button_id=KNAR2VKYRYUV6)
