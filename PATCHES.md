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
| `rust-toolchain.toml` | Pin the Rust toolchain. **Effective as of phase 1** — see "The toolchain, and how it stays out of the way" below. |
| `crates/harness/` | `plato-harness`: the permanent headless smoke test. Opens an EPUB, lays it out at 1072×1448 @ 300 dpi, writes a PNG, exits. It is the thing that gets run natively *and* under `qemu-arm` so the two renders can be diffed; phase 0's equivalent was a throwaway. Also a workspace member (one line in `Cargo.toml`). |
| `csupport/c23_math_compat.c` | The four C23 libm functions musl does not ship. See below. |

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

### `crates/core/build.rs` — a branch for `armv7-unknown-linux-musleabi`

Upstream's `build.rs` knows two shapes: Kobo hard-float, or a host with a
*dynamic* `libstdc++`/`libc++`. Static musl is neither, so the fork adds one
early-returning branch (and touches nothing else in the file):

- **no C++ runtime.** harfbuzz is compiled `-fno-exceptions -fno-rtti`, and
  `llvm-nm -u` on the resulting archive shows no C++ runtime symbols at all —
  checked, not assumed. Linking `libc++` would buy a dependency for nothing.
- **no bzip2.** freetype is configured `--with-bzip2=no` and nothing else in
  the EPUB path wants it, so upstream's unconditional `-lbz2` is dropped here.
- **everything static**, because a `dylib=` entry cannot work at all.
- plus `-lc23compat`, for the shim below.

### `csupport/c23_math_compat.c` — `fminimum_num*` / `fmaximum_num*`

Not a Plato problem and not a zig problem. **rustc 1.97 lowers `f32::min` /
`f32::max` to the C23 functions `fminimum_numf` / `fmaximum_numf`. glibc has
had them since 2.35; musl 1.2 does not.** So every Rust binary containing a
float min or max fails to link for any `*-linux-musl*` target:

```
ld.lld: error: undefined symbol: fminimum_numf
>>> referenced by ... paragraph_breaker::total_fit ...
```

Four functions, written to the C23 wording — including the two things `<` does
not give you: a NaN argument loses to a number, and `-0.0 < +0.0`. Worth
knowing beyond this fork: **it will bite any Rust + musl cross build**, and it
would equally bite the `gnueabi.2.20` fallback, since the device's glibc is far
older than 2.35.

## The kindle profile: what it does, and the walls found building it

`python3 xbuild.py kindle` builds nine C libraries as static archives into one
staging prefix, then cross-links plato-core and the harness through
`cargo-zigbuild`, then runs an ABI gate. It replaces `thirdparty/build.sh`,
`thirdparty/download.sh` and every `thirdparty/*/build-kobo.sh` **for our
target only** — upstream's scripts are still there, untouched, for rebasing.

### The toolchain, and how it stays out of the way

Two toolchains, both pinned, glued by `cargo-zigbuild`:

- `zig cc` / `zig c++` 0.16.0, `-target arm-linux-musleabi
  -mfloat-abi=softfp -mcpu=cortex_a9`, written down in exactly one place
  (`ZIG_ARCH_FLAGS`) and exported to configure/cmake/make as four two-line
  wrapper scripts, because none of those three survive a `$CC` with spaces.
- rustup, pinned by `rust-toolchain.toml` (1.97.1) + target
  `armv7-unknown-linux-musleabi`, installed with
  `rustup toolchain install 1.97.1 && rustup target add --toolchain 1.97.1
  armv7-unknown-linux-musleabi`, and `brew install cargo-zigbuild`.

**This Mac's default `cargo`/`rustc` are Homebrew's and other projects depend
on them, so nothing here changes the global PATH or rustup's default
toolchain.** `xbuild.py` asks `rustup which cargo` / `rustup which rustc` from
the repository root — so `rust-toolchain.toml` decides — and invokes both by
absolute path.

That second one is not belt and braces. Homebrew's rustup shim is a bash
wrapper, and a cargo reached through it still resolves `rustc` from `PATH` —
i.e. Homebrew's rustc, which has no cross targets. The symptom is a very
convincing lie:

```
error[E0463]: can't find crate for `core`
  = note: the `armv7-unknown-linux-musleabi` target may not be installed
```

about a target that *is* installed. Setting `RUSTC` explicitly is the fix.

### softfp, and the NEON wall behind it

`-mfloat-abi=softfp` keeps the soft-float **calling convention** the device
requires (`e_flags 0x5000200`, same as every Amazon binary) while still letting
the compiler emit VFPv3/NEON instructions — the same trade koxtoolchain makes
for kindlepw2. Verified both ways: VFP instructions in the disassembly, and the
ABI gate on every produced binary.

**But hand-written NEON does not survive it.** clang cannot lower
`<arm_neon.h>` vector types for a soft-float-ABI target:

```
fatal error: error in backend: Do not know how to split this operator's operand!
```

and under plain soft-float `arm_neon.h` is not even usable (`__bf16 is not
supported on this target`). Under `musleabihf` the same file compiles fine, so
this is the float ABI, not zig. Auto-vectorisation is unaffected; only
intrinsics break. Exactly two libraries in the stack have them, and both expose
a switch, so this costs **no patch**:

- libpng: `--enable-arm-neon=no` (its filter fast paths; libpng is only here
  for freetype's PNG-in-font glyphs).
- MuPDF: `-DARCH_HAS_NEON=0`, which `include/mupdf/fitz/system.h` guards with
  `#ifndef` precisely so it can be overridden.

Worth carrying into phase 2: if a hot loop ever wants NEON intrinsics, it has
to be written as auto-vectorisable C, or built as a separate `musleabihf`
object, which we cannot link. Not a problem today.

### MuPDF: `OS=kindle`, and why the name is load-bearing

`Makerules` sets `HAVE_OBJCOPY := yes` **only** for `OS=Linux`, and with
objcopy the embedded fonts get ELF-style
`_binary_resources_fonts_..._start` symbol names. Without it MuPDF falls back
to `scripts/hexdump.sh`, which emits the short `_binary_DroidSansFallback_ttf`
names — and those short names are exactly what
`crates/core/src/font/mod.rs` declares under `target_arch = "arm"`. Upstream
gets this by accident from `OS=kobo`; here it is deliberate and commented.

No `kobo.patch` equivalent is needed: the compiler, archiver and every
`SYS_*_CFLAGS`/`SYS_*_LIBS` go on the make command line, as the host profile
already did. `make generate` still runs with the host compiler, as MuPDF's own
Makerules instructs.

MuPDF's TOFU font macros are deliberately **not** set, unlike upstream's kobo
build. Upstream needs them because it links one shared object and pays for
every font in it; we link static archives, so an unreferenced font object is
simply never pulled in — and skipping the macros keeps the host and ARM builds
byte-identical, which is what makes the render comparison meaningful.

### The rest of the C stack

- **harfbuzz**: `src/harfbuzz.cc` is an amalgam of the whole library, so one
  `zig c++` invocation replaces upstream's meson cross-file build. **meson and
  ninja are not needed at all.** `-DHAVE_FREETYPE=1` is what pulls in
  `hb-ft.cc`, the only part Plato's FFI touches.
- **gumbo**: compiled directly from its nine C99 files; the GitHub archive
  ships only `configure.ac`, so the alternative was an autoreconf dependency.
- **zlib**: needs `--uname=Linux`. Its configure is not autoconf and picks the
  archiver from `uname`; on macOS that is Apple's `libtool`, which rejects ARM
  ELF objects with `adler32.o is not an object file`.
- **libpng, libjpeg (IJG v9f), jbig2dec, freetype**: plain autotools with
  `--host=arm-linux-musleabi`, `--disable-shared`, `$CC`/`$AR`/`$RANLIB`
  pointed at the zig wrappers. No patches.
- **openjpeg**: cmake, same wrappers.
- **djvulibre and bzip2 are gone** — djvu is feature-gated off, bzip2 has no
  consumer once freetype is built `--with-bzip2=no`. Eleven upstream libraries
  become nine.

All nine are sha256-pinned over https. Upstream's `download.sh` pins nothing
and fetches libjpeg and djvulibre over plain http; `build.sh fast` downloads
prebuilt binaries from the maintainer's server. Neither path is used here.

### The ABI gate

`check_arm_abi()` runs on every ARM binary before the build is allowed to
succeed: ELF magic, `e_machine == ARM`, and `EF_ARM_ABI_FLOAT_HARD` clear. It
is the same check as `scripts/check-arm-abi.py` in the ezkindle repo,
reimplemented here so the fork has no path dependency on a sibling checkout —
and if that checkout *is* next door, xbuild runs it too, so the two can never
silently disagree.

## Deliberately *not* changed (yet)

- `.cargo/config.toml` still hardcodes the hard-float Kobo linker. It is not
  read by either of our profiles (both name their target explicitly and go
  through `cargo-zigbuild`), so it is left alone for rebasability rather than
  edited.
- `crates/core/src/document/mupdf_sys.rs` pins `FZ_VERSION = "1.27.0"`, which is
  why `xbuild.py` builds MuPDF **1.27.0** from source rather than using
  Homebrew's. `fz_new_context_imp` checks that string at runtime.
- The Kobo device model ladder, the framebuffer, input — all phase 2.
