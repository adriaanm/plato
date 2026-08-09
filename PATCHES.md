# PATCHES.md — the `ezkindle` fork's divergence from upstream Plato

Branch `ezkindle`, forked from `baskerville/plato@7da89f2` (2026-08-09).

This fork exists to port Plato to a jailbroken **Kindle Paperwhite 3** — see
`docs/plato-port.md` in the `ezkindle` repo. It is meant to stay **rebasable on
upstream master**, so every divergence is listed here with its rationale, and
each one is either (a) plausibly mergeable upstream or (b) confined to files
upstream does not have.

The rule: touch the *build path* freely, touch `crates/` as little as possible.
Explicitly untouched, so a rebase never fights: `contrib/*.sh`, `scripts/*.sh`,
every `thirdparty/*/build-kobo.sh` and `kobo.patch`, `build.sh`, `dist.sh`,
`download.sh`, `service.sh`, `run-emulator.sh`, `mupdf_wrapper/build*.sh`.

## New files (no upstream counterpart — rebase-safe by construction)

| File | Why |
|---|---|
| `xbuild.py` | The one build driver. Takes over the *roles* of `thirdparty/download.sh` and `thirdparty/build.sh`: a declarative table of {lib, version, url, sha256, build steps}, sha256-verified downloads, per-profile builds. Repo rule: scripts are Python, not bash. Upstream's scripts are left in place untouched for reference and for rebasing. |
| `PATCHES.md` | This file. |
| `rust-toolchain.toml` | Pin the Rust toolchain. **Not yet effective**: this Mac has Homebrew's `rustc`, which ignores `rust-toolchain.toml` entirely — only rustup honours it. The pin becomes real in phase 1, when rustup + `cargo-zigbuild` + `armv7-unknown-linux-musleabi` arrive. Recorded here so the file is not mistaken for a working guarantee. |

## Modified files

### `.gitignore` — ignore `/.xbuild/`

One line. `xbuild.py` keeps all of its state (verified tarballs, extracted
sources, C build trees) under `.xbuild/`, never in `thirdparty/`.

### DjVu behind a cargo feature, default **off**

Files: `crates/core/Cargo.toml`, `crates/core/src/document/mod.rs`,
`crates/core/src/metadata.rs`.

Per the 2026-08-09 "EPUB first" decision, DjVu is out of scope, and gating it
removes **djvulibre from the build entirely** — one fewer C library to
cross-compile in phase 1, and one fewer over-plain-HTTP download.

The diff is 12 lines and exactly the shape the port map predicted: a new
`[features]` block (`default = []`, `djvu = []`), plus `#[cfg(feature = "djvu")]`
on two module declarations, two `use` lines and two match arms. Nothing else in
the tree mentions DjVu except three incidental things left alone:

- `settings/mod.rs` lists `"djvu"` in the default `metadata_kinds` /
  `allowed_kinds` — harmless strings, and touching them would be a behaviour
  change, not a build change.
- `view/reader/mod.rs` has a `#djvu_page`-style link regex — pure text parsing,
  no djvulibre.
- `document::file_kind` still sniffs the `AT&T` magic and returns `"djvu"`. With
  the feature off, that falls through to the `_` arm and MuPDF declines the
  file, so a DjVu opens as "can't open" rather than being misdetected. Left as
  is deliberately: it keeps the patch to `#[cfg]` attributes only, with no
  control-flow edits, which is what makes it rebasable.

**This one is a candidate to upstream**: cargo features for the optional
document backends are useful to anyone building Plato, and this change is
additive with the default preserving today's behaviour for anyone who opts in.
(Upstream's default would presumably be `default = ["djvu"]`; ours is off.)

## Deliberately *not* changed (yet)

- `.cargo/config.toml` still hardcodes the hard-float Kobo linker. Phase 1
  replaces it; the emulator path never reads it.
- `crates/core/src/document/mupdf_sys.rs` pins `FZ_VERSION = "1.27.0"`, which is
  why `xbuild.py` builds MuPDF **1.27.0** from source rather than using
  Homebrew's. `fz_new_context_imp` checks that string at runtime.
- The Kobo device model ladder, the framebuffer, input — all phase 2.
