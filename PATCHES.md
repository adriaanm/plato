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
| `crates/core/src/document/layout.rs` | Page-layout analysis: content boxes and their aggregation. Pure functions over slices; no `Reader`, no MuPDF, no I/O. Phase A of the PDF work — see below. |
| `crates/core/tests/pdf_layout.rs` | The same analysis through the real FFI, skipped unless `PLATO_TEST_PDF` names a PDF. |
| `crates/core/test-data/{line-boxes.json,gen-line-boxes.py}` | Extracted fz_stext boxes from three real papers, and the script that extracts them. |

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

## Phase 2: the Kindle backend

`docs/plato-port.md`'s phase 2, written **blind** — the device was not in hand.
Correctness comes from the reference material plus compile-time assertions and
unit tests; nothing here has been observed on hardware yet, and
`PLATO-DEVICE-PROBES` (phase 3) is the ticket that upgrades it.

The shape of the diff is the one the port map predicted: **four new files that
upstream has no counterpart for**, plus small, listed branches in four existing
ones. `kobo1.rs` and `kobo2.rs` are not touched at all.

### New files

| File | Why |
|---|---|
| `crates/core/src/framebuffer/kindle_mxcfb_sys.rs` | The lab126 mxcfb dialect: `struct mxcfb_update_data` (72 bytes, with the `hist_*_waveform_mode` pair between `update_marker` and `temp`, and an alt buffer with no `virt_addr`), the ioctl numbers, the waveform constants and the EPDC flags. |
| `crates/core/src/framebuffer/kindle.rs` | `KindleFramebuffer` and `refresh_policy`. |
| `crates/core/src/battery/kindle.rs` | `KindleBattery`. |
| `crates/core/src/frontlight/kindle.rs` | `KindleFrontlight`. |

### The header vs. the delta table

Both were read in full and cross-checked. **They agree on everything the port
depends on** — struct layout, the three ioctl numbers, `TEMP_USE_AUTO`, and the
whole REAGL renumbering. Two things the header says that the table does not, both
recorded rather than acted on:

- **`EPDC_FLAG_USE_DITHERING_Y4` is `0x8000` on lab126**, which is
  `EPDC_FLAG_USE_REGAL` in Plato's NTX header, and `0x4000` is `_Y2` here where
  it is `_Y4` there. Harmless for us — we set no dithering flag at all — but it
  is exactly the kind of collision that makes sharing one constants module
  between the two dialects a bad idea, and it is why they are separate files.
- **`TEMP_USE_PAPYRUS` is also `0x1001`**, i.e. `TEMP_USE_AUTO` on the PW2+ *is*
  the Touch/PW1 constant under a new name. KOReader sets it unconditionally on
  the `refresh_k51` path for that reason, and so do we.

The table's "PW3 wait ioctl is identical to Plato's `wait_for_update_v2`" is
confirmed by the header: `_IOWR('F', 0x2F, struct mxcfb_update_marker_data)`,
and that struct is byte-identical to Plato's `MxcfbUpdateMarkerData`.

### The compile-time safety net

`kindle_mxcfb_sys.rs` asserts, in `const` context, that the struct is 72 bytes,
that every field is at the offset the header puts it at (so "72 bytes" cannot be
satisfied by the *wrong* 72 bytes — the Rex variant appends the `hist_*` pair
instead of inserting it, and is also 72), and that the three ioctl numbers come
out as `0x4048462e`, `0xc008462f` and `0x40044637` — the values KOReader quotes.
These fire on the host build and on the armv7 build alike.

The `_IOC` encoding is **written out here rather than taken from `nix`**. `nix`'s
`ioctl_*!` macros encode BSD-style on macOS, so asserting on their output would
be asserting something different on the host than on the device — i.e. checking
nothing where it matters. `KindleFramebuffer` therefore calls `libc::ioctl` with
these constants directly, instead of going through `nix` as `kobo1.rs` does.
That divergence is deliberate and is the whole point of the exercise.

### The refresh policy

`refresh_policy(mode, monochrome, inverted) -> RefreshPolicy` is a pure function,
so the part that was written blind is the part that is unit-tested (seven tests).
It is KOReader's `refresh_k51` + `mxc_update`, restricted to what the PW3
branch of `framebuffer:init()` configures:

| `UpdateMode` | waveform | update mode | fences |
|---|---|---|---|
| `Gui`      | `GC16_FAST` | `PARTIAL` | submission before |
| `Partial`  | `REAGL`     | **`FULL`**, promoted | complete before, complete after |
| `Full`     | `GC16`      | `FULL`    | submission + complete before, complete after |
| `Fast`     | `DU`        | `PARTIAL` | — |
| `FastMono` | `DU`        | `PARTIAL` | — (+ `FORCE_MONOCHROME`) |

plus `hist_bw = DU`, `hist_gray = GC16_FAST`, except all-REAGL on a REAGL send
and `hist_gray = GC16` on a GC16 send; `temp = TEMP_USE_AUTO`;
`collision_test = 0`; never `EPDC_FLAG_TEST_COLLISION`; no hardware dithering;
and updates with `w <= 1 || h <= 1` discarded before the ioctl.

Three deliberate divergences from KOReader, all in the same direction — keep
Plato's semantics where KOReader's would be a behaviour change:

- **`FORCE_MONOCHROME` is not set on every DU update.** `refresh_k51` sets it
  whenever `waveform_mode == DU`; Plato distinguishes `Fast` from `FastMono`, and
  collapsing them would silently crush antialiasing on every fast update. So the
  flag follows Plato's `FastMono` (and sticky `set_monochrome`), not the waveform.
- **The "full-screen flashing UI" clause of the complete-before fence is not
  reproduced.** It is unreachable here: Plato's only flashing intent is `Full`,
  which is GC16, and the GC16 clause already covers it.
- **Inversion is `EPDC_FLAG_ENABLE_INVERSION` only**, as on a Kobo of mark < 11.
  KOReader instead swaps in `GL16_INV` / `GC16` as night waveforms. That is a
  quality refinement on identical pixels; guessing at it blind buys nothing, and
  the constant is in the sys module ready for phase 4 to try.

### Rotation: refused, not faked

`KindleFramebuffer::set_rotation` never writes `FBIOPUT_VSCREENINFO`, and returns
`Err` for any rotation but the panel's own. KOReader treats every Kindle
framebuffer as fixed-orientation, and Amazon's own reader does not write the
rotate field either.

`Err` rather than "`Ok` with unchanged dims" is the load-bearing choice, and it
is correct because of how the callers are written: all six `set_rotation` sites
in `app.rs` (plus the one in `view/rotation_values`) are guarded by
`if let Ok(dims) = …` or `.ok()`, so an `Err` leaves `context.display` untouched
— dims, rotation, and therefore the input-side axis transform stay consistent
with the panel. Returning `Ok` would let a caller record a rotation the hardware
does not have, and the touch mapping would silently follow it.

Real software rotation is a self-contained later addition; nothing here blocks it.

### The `mark()` / capability audit

`Model::KindlePaperwhite3` answers **`mark() == 6`**. Every site that consults
`mark()` or a capability predicate was read, and the choice checked against it:

| Site | Consults | Reached on the PW3? | Why the value is right |
|---|---|---|---|
| `plato/src/app.rs:212` | `mark() != 8` | **No** — the `is_kindle()` branch is placed *before* it | This is the one that would matter: mark 6 selects `KoboFramebuffer1`, i.e. a 68-byte struct through ioctl `0x4044462e`. The Kindle branch comes first. |
| `framebuffer/kobo1.rs` (×8) | `mark()`, `model`, `color_samples()` | No — `KoboFramebuffer1` is never constructed | Untouched by this phase, as required. |
| `framebuffer/kobo2.rs:64` | `startup_rotation()` | No — `KoboFramebuffer2` is never constructed | — |
| `frontlight/premixed.rs:49` | `mark() != 8` | No — `PremixedFrontlight` is never constructed | `frontlight_kind()` is `Standard`, *and* `app.rs` branches to `KindleFrontlight` before consulting it at all. |
| `frontlight/natural.rs:40` | `model` | No — `NaturalFrontlight` is never constructed | Same. |
| `input.rs:402` | `proto == Single && mark() == 3` | Yes, but short-circuits | The PW3 is `MultiB`. Any `mark()` other than 3 is safe; 6 is. |
| `input.rs:503` | `transformed_gyroscope_rotation()` | No | `has_gyroscope()` is false. |
| `input.rs:370-372` | `should_swap_axes()`, `should_mirror_axes()` | **Yes** | See below — this is the one that actually constrains the ladder. |
| `document/mod.rs:485` | `mark()` | Yes | Cosmetic: the system-info page prints "Mark 6". |
| `document/mod.rs:480`, `app.rs:327` | `model` | Yes | Prints "Kindle Paperwhite 3". |
| `document/mod.rs:516` | `INTERNAL_CARD_ROOT` | Yes | Now `/mnt/us/books`; `statvfs` reports the userstore. |
| `context.rs:56`, `app.rs:218` | `transformed_rotation()` | Yes | Default arm, identity. |
| `app.rs:113`, `view/frontlight.rs:250,300`, `view/reader/mod.rs:2801` | `has_lightsensor()` | Yes | False → the existing null `LightSensor`, no auto-brightness UI. Correct: no ALS on a PW3. |
| `view/frontlight.rs:51,92,213,342,369` | `has_natural_light()` | Yes | False → intensity slider only, no warmth. Correct: one white channel. |
| `view/common.rs:115` | `has_page_turn_buttons()` | Yes | False. Correct. |
| `view/common.rs:124`, `app.rs:220,988` | `has_gyroscope()` | Yes | False. `app.rs:220` and `:988` then compare rotations that are already equal, so no `set_rotation` call is even attempted at startup or exit. |
| `battery/kobo.rs:40` | `has_power_cover()` | No — `KoboBattery` is never constructed | — |
| `view/{intermission,home/shelf,home/book,reader,dictionary}`, `document/pdf.rs` | `color_samples()` | Yes | 1 → grayscale pixmaps. Correct for this panel. |
| `view/{calculator,home,reader}` | `mirroring_scheme()` (only `dir`) | Yes | Default `(2, 1)`. Used to turn a corner tap into a rotation request, which `set_rotation` refuses. |
| `has_removable_storage()` | — | — | No consumer anywhere in `crates/`; false regardless. |

**The one that constrains the ladder is `input.rs:370-372`.** Plato swaps
`ABS_MT_POSITION_X`/`_Y` whenever `should_swap_axes(rotation)`, because every
Kobo's touch panel is landscape-native while its display is portrait. The PW3's
is portrait-native — KOReader applies *no* coordinate transform. So the Kindle
takes `startup_rotation() == 0` with the **default** swapping (1) and mirroring
((2, 1)) schemes, which makes `should_swap_axes(0)` false and
`should_mirror_axes(0)` `(false, false)`: the identity.

The price is that `orientation(0)` reads as `Landscape`. That is a real
mismatch with a 1072×1448 portrait panel, and it is accepted because it is
**unobservable here**: all three consumers of `orientation()` either require a
gyroscope this model does not have (`app.rs:549`) or merely guard a
`set_rotation()` call that `KindleFramebuffer` refuses (`app.rs:744`, `:851`).
Touch correctness is the thing that would actually break, so it wins. Recorded
rather than hidden, because a future software-rotation implementation has to
revisit exactly this.

### Modified files

- **`crates/core/src/device.rs`** — `Model::KindlePaperwhite3`, an
  `is_kindle()` predicate, and arms in `mark()` and `startup_rotation()`.
  Detection is `Device::new` → `Device::detect`, which checks
  **`PLATO_DEVICE=kindle-pw3` before the `PRODUCT` match**. An env var rather
  than a sniff: a Kobo exports `PRODUCT` from its own init and a Kindle exports
  nothing, so any heuristic would be a guess that, if it ever misfired on a
  Kobo, would send Kobo hardware a 72-byte update struct. Split into
  `new`/`detect` so precedence is unit-testable without touching the process
  environment.

- **`crates/core/src/framebuffer/mod.rs`** — two `mod` lines and one `pub use`.

- **`crates/core/src/{battery,frontlight}/mod.rs`** — one `mod` and one
  `pub use` each.

- **`crates/core/src/settings/mod.rs`** — `DEFAULT_FONT_PATH`,
  `INTERNAL_CARD_ROOT` and `EXTERNAL_CARD_ROOT` become `lazy_static`
  `&'static str` instead of `const &str`, chosen per device.
  **The Kobo values are byte-identical**; the Kindle gets `/mnt/us/fonts`,
  `/mnt/us/books` and `/mnt/us`. `&'static str` rather than a function is the
  smallest mechanism that leaves every use site alone apart from a `*` deref
  (three sites, one of them in `document/mod.rs`), and nothing downstream learns
  that these are now computed. The two `/mnt/onboard/.kobo/{dropbox,kepub}`
  library defaults are deliberately left hardcoded — they are Kobo-specific
  services, not storage roots, and rewriting them would be a behaviour change
  rather than a path change.

- **`crates/plato/src/app.rs`** — three `is_kindle()` branches (framebuffer,
  battery, frontlight), each placed *before* the Kobo selection it would
  otherwise fall into, plus imports. `LightSensor` is untouched: the existing
  `has_lightsensor()` guard already yields the null impl.

  Input: `/dev/input/event1` is the PW3's `cyttsp4_mt_b` node and the existing
  `TOUCH_INPUTS` fallback list already ends in it — Kobo's `by-path` entries do
  not exist on a Kindle, so the loop falls through. No change needed, only a
  comment. `POWER_INPUTS` carries a marked `TODO(PLATO-DEVICE-PROBES)`: all
  three entries are Kobo PMIC nodes, KOReader's Kindle frontend names no power
  input path either (it takes the button through `powerd` over lipc), so there
  is nothing to copy and nothing is invented. Missing it costs nothing at first
  light — the button still suspends via Amazon's `powerd`, Plato just does not
  see the event.

- **`xbuild.py`** — `Profile.cargo_package` becomes `cargo_packages`, and the
  kindle profile now builds **`plato` (the real device binary) *and*
  `plato-harness`**, both through the ABI gate. `--package` is repeatable. The
  SDL emulator stays host-only and is untouched.

### Results

```
cargo test -p plato-core                        46 passed, 0 failed
python3 xbuild.py kindle                        both binaries, ABI gate passed
  target/armv7-unknown-linux-musleabi/release/plato          50 757 KiB
  target/armv7-unknown-linux-musleabi/release/plato-harness  48 037 KiB
  e_flags=0x5000200 (soft-float) on both
```

The size is MuPDF's embedded fonts, as in phase 1, and is the same ~43 MB that
was already noted as trimmable.

## Phase 3: what the device said, and the two things it changed

`PLATO-DEVICE-PROBES` ran 2026-08-09 (raw output in ezkindle
`device-facts/plato-phase3-probes.txt`, plus a 583-event touch capture in
`device-facts/touch-capture-event1.raw`). Seven of the nine checklist items
below came back as written. Two did not, and this is what they cost.

### Touch: `TouchProto::MultiSlot`, a genuinely new protocol mode

Files: `crates/core/src/input.rs`, `crates/core/src/device.rs`,
`crates/core/test-data/touch-capture-event1.raw` (new).

The panel is `cyttsp4_mt` on `/dev/input/event1` and its ABS bitmap is exactly
`ABS_MT_SLOT(47)`, `ABS_MT_POSITION_X(53)`, `ABS_MT_POSITION_Y(54)`,
`ABS_MT_TRACKING_ID(57)` — **no `ABS_MT_PRESSURE`, no plain `ABS_X`/`ABS_Y`**.

Every one of Plato's existing multi-touch modes keys a contact on a pressure-ish
axis and ignores `ABS_MT_SLOT` outright: `MultiA` on `ABS_MT_TOUCH_MAJOR`,
`MultiB` and `MultiC` on `ABS_MT_PRESSURE` (they differ only in whether a
release is inferred from pressure or from a contact vanishing between packets).
On this panel that axis never arrives, so **every existing path sees a screen
nobody ever touches.** Phase 2's `MultiB` guess, made from the driver's module
name, was wrong in the way that produces silence rather than an error.

**The name.** `MultiC` was the obvious candidate and it is taken: it means
"pressure indicates release", i.e. the `MultiB` codes minus the release sweep,
and it is live on four Kobos (Elipsa, Sage, Libra 2, Elipsa 2E). Renumbering
the letters would touch Kobo rows for no gain, and reusing the name would make
`europa` and the PW3 mean different things by the same word. The new variant is
therefore `MultiSlot` — named for the mechanism rather than the next free
letter, because that mechanism is the whole distinction: it is the only mode
that reads `ABS_MT_SLOT` at all.

**The semantics** are the kernel's protocol B verbatim
(`Documentation/input/multi-touch-protocol.txt`): the driver keeps per-slot
state and sends **only what changed**; `ABS_MT_SLOT` selects the current slot;
`ABS_MT_TRACKING_ID >= 0` opens a contact and `-1` closes it; positions update
the current slot; **state persists across `SYN_REPORT`**. Finger identity, as
`gesture.rs` consumes it, is the slot's current tracking id. Pressure appears
nowhere.

**The shape of the patch** is chosen so no other device can be affected. The
state machine is a separate `MultiSlotTracker`, and `parse_device_events` holds
it in an `Option` that is `Some` for `MultiSlot` and `None` for everything else:

```rust
let mut slots = (proto == TouchProto::MultiSlot).then(MultiSlotTracker::new);
```

Two `if let Some(slots)` arms (one in `EV_ABS`, one at `SYN_REPORT`) short-
circuit ahead of the existing code. On any Kobo `slots` is `None`, so the code
that runs is byte-for-byte what ran before — and `device.rs` now pins every
Kobo product's protocol in a test, so the new variant cannot drift onto
hardware it was not written for.

Splitting the tracker out is also what makes it testable: the device holds
`EVIOCGRAB` on its touch node whenever KOReader runs, so live capture is
expensive, and this is a state machine that deserves to be pinned rather than
eyeballed.

Three details the live capture settled, all of which the tracker handles:

- **A lone finger never sends `ABS_MT_SLOT`.** Slot 0 is implicit, so the
  tracker starts on slot 0 rather than waiting to be told.
- **`ABS_MT_TRACKING_ID` is sent on change only** — once at contact start, then
  not again until the `-1`. A parser needing it per packet sees one frame of a
  swipe and then nothing.
- **A lift can arrive as a bare `SLOT n` + `TRACKING_ID -1`**, with no
  coordinates, in either slot order. The `Up` therefore carries the slot's last
  known position, and pending `Up`s are emitted in slot order regardless of the
  order they arrived in, so the output is deterministic.

Also ignored: `BTN_TOOL_FINGER(325)` and `BTN_TOOL_DOUBLETAP(333)`, which this
driver sends and which would otherwise surface as `ButtonCode::Raw` presses on
every tap. Gated on `MultiSlot`, so no Kobo's button stream changes. `BTN_TOUCH`
was already ignored upstream.

**Tests** (13 new): eleven synthetic streams — implicit slot 0; interleaved
slots with the id sent only on change; a conservative driver that re-sends
nothing after contact start; a bare `SLOT/-1` lift; both fingers lifting in one
packet in reverse order; a contact that opens and closes inside one frame
(still a tap); a new tracking id with no intervening `-1` (an implicit lift);
an out-of-range slot; an idle frame emitting nothing; mirroring — plus a replay
of the 583-event live capture asserting the 14 contacts actually performed: 8
corner taps in TL/TR/BR/BL order twice, 2 two-finger taps (detected as
overlapping strokes), 2 rightward swipes, every coordinate on-panel, and **no
phantom fingers left open at end of stream**.

The capture is committed at `crates/core/test-data/touch-capture-event1.raw`:
9 KB, and device-anonymous (coordinates and timestamps only — and the clock is
wrong anyway, this device has never had a network).

### `vinfo.rotate == 3`, and why nothing had to change

File: `crates/core/src/framebuffer/kindle.rs`.

The panel reports `rotate = 3` with `xres`/`yres` already `1072`/`1448`, and the
capture shows touch coordinates that are portrait-native screen pixels with no
transform (the top-left corner tap reads `(64, 62)`).

Checklist item 2 above said "if the panel reports something other than 0, the
constant in `device.rs` is what changes". **That would have been wrong**, and
recording why is the point of this section. Plato's rotation is not a hardware
register; it is the value `Device::should_swap_axes` and `should_mirror_axes`
consume to transform touch input. `should_swap_axes(3)` is true under the
default swapping scheme, so recording a 3 would swap `ABS_MT_POSITION_X`/`_Y`
and mirror both axes — breaking touch on a panel whose coordinates are already
correct. lab126's `rotate` describes the EPDC's own scanout in its own
numbering, and it does not map onto Plato's `0..4` at all. KOReader, which
drives this panel correctly, likewise never reads the field.

So the invariant is unchanged and now explicit: **`dims()` is `xres`/`yres` =
1072×1448, input coordinates pass through untransformed, and the rotation Plato
records for this panel is `startup_rotation() == 0` everywhere** — in the
framebuffer (constructed with it, `set_rotation` refuses to move off it), in
`Device` (identity swap and mirroring at 0), and in `input.rs` (which derives
its transform from exactly those two calls). Self-consistent by construction;
no code change was required.

What *was* added is a guard on the assumption that would actually hurt if it
broke. `check_geometry(xres, yres, rotate)` refuses a panel reporting landscape
geometry — that, not the rotate value, is what would silently render sideways
and misplace every touch. Two tests: any `rotate` is accepted with portrait
dims, transposed dims are an error.

One stale comment corrected while here: `set_rotation`'s doc claimed the Kindle
takes `swapping_scheme() == 0`. It takes the default, **1**. With 0,
`should_swap_axes(0)` would be true and the axes would swap — the exact
opposite of what the comment existed to justify. The code was always right; the
comment was not.

### Frontlight: 4095, and a fallback that was quietly wrong

File: `crates/core/src/frontlight/kindle.rs`.

`max_brightness` reads **4095** — not the 24/25 the Kindle folklore quotes, and
not the 255 a sysfs backlight is usually assumed to have. `KindleFrontlight`
already read the file at construction, so the scaling itself needed no change,
which is the outcome that design was for.

But the fallback constant used when the file is unreadable was `24`, and 24 out
of 4095 is under 1% — a "working light with a slightly wrong ceiling" would in
fact have been a light that appears broken. It is now 4095, and 4095 is in both
scaling test tables (the round trip through a percentage still hits every one
of the 4096 raw steps exactly).

### Ready for phase 4

Every item on the phase-3 checklist is closed: seven confirmed as written, two
turned into the code above. The device binary builds and passes the ABI gate,
and `cargo test -p plato-core` is at **62 passed, 0 failed** (46 at the end of
phase 2). Nothing on the input, rotation or frontlight paths is a guess any
more — the remaining unknowns are all things only a running binary can answer.

### What phase 3 must confirm before first light

*Kept as written at the end of phase 2 — the answers are above.*

Every one of these is read-only, and each turns a *Likely* in this phase into a
*Confirmed* (or into a two-line fix):

1. **`FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO`** — `bits_per_pixel` (8
   expected), `xres`/`yres` (1072×1448), and `line_length` (1088 expected, i.e.
   *not* `xres`). The code already trusts `line_length` and picks its accessors
   from `bits_per_pixel`, so this confirms rather than configures — except that
   a `bits_per_pixel` that is not a multiple of 8 is a hard error today.
2. **`var_info.rotate`** as read at startup. `KindleFramebuffer` is constructed
   with `startup_rotation()` (0) and refuses anything else; if the panel reports
   something other than 0, the constant in `device.rs` is what changes.
3. **Touch: `evtest`/`getevent` on `/dev/input/event1`.** The open question from
   `docs/plato-port.md`: does `cyttsp4_mt_b` interleave slots without re-sending
   `ABS_MT_TRACKING_ID` each packet? Plato's protocol-B handling keys on the
   tracking ID alone and ignores `ABS_MT_SLOT`. If slots are interleaved, `input.rs`
   needs slot state — the only change in this whole port that is not yet written.
4. **Touch orientation.** Confirm X runs along 1072 and Y along 1448, with no
   mirroring, i.e. that the identity transform chosen above is right.
5. **`/dev/input/event1` is actually the touch node**, and what the other
   `/dev/input/event*` nodes are (`/proc/bus/input/devices`) — specifically
   which one, if any, carries the power button.
6. **`ls /sys/class/power_supply/`** — the node name, and whether it has both
   `capacity` and `status`. `KindleBattery` globs and sorts, so it survives any
   single node; what it cannot survive is the wario tree
   (`/sys/devices/system/wario_battery/…/battery_capacity`) being the only
   source, which is the documented fallback in that file.
7. **`cat /sys/class/backlight/max77696-bl/max_brightness`** — and that the node
   exists at all under that name. The scaling reads it at init and falls back to
   24 with a warning.
8. **`MXCFB_GET_WAVEFORM_TYPE` / `MXCFB_GET_TEMPERATURE`** — cheap, read-only,
   and they prove the ioctl numbering is right *before* anything writes a frame.
   This is the safest possible first contact with the EPDC.
9. **`/sys/devices/platform/falconblk`** — hibernation, per the phase-3 plan.
   Not needed by this phase; recorded so the one ssh session covers it.

## Phase 5 — the powerd integration

Design and evidence live in the `ezkindle` repo (`docs/plato-port.md`, section
"powerd integration"). The device-side shell — `suspend.sh`, `resume.sh` and a
`plato.sh` launcher speaking lipc to `com.lab126.powerd` — lives there too,
under `device/plato/scripts/`, **not** in this fork: upstream's `scripts/*.sh`
are Kobo's and stay untouched so a rebase never fights, and the Kindle hooks
are deployment payload rather than app source.

That leaves exactly two diffs here.

### `crates/plato/src/app.rs` — the Kindle carries no `Rtc`

```rust
let rtc = if CURRENT_DEVICE.is_kindle() { None } else { Rtc::new(RTC_DEVICE)… };
```

`/dev/rtc0` exists on the PW3, and `rtc.rs` would happily drive it — which is
the problem. powerd owns the wake alarm: it is set through the `rtcWakeup` lipc
property (and only while powerd is in `readyToSuspend`), and powerd programs
the chip through
`/sys/devices/platform/imx-i2c.0/i2c-0/0-003c/max77696-rtc.0/rtc_delta_alarm`
— confirmed from `/etc/kdb/system/daemon/powerd/SYS_RTC_WAKEUP` — not through
the RTC ioctls `rtc.rs` uses. Two writers to one alarm register is a silent
fight with the device's own suspend policy.

With `rtc == None` every `auto_power_off` branch in `Event::Suspend` becomes a
no-op (`context.rtc.iter()` yields nothing; the `and_then` short-circuits), so
this removes a footgun rather than a feature — `auto-power-off` is 0.0 in the
device's `Settings.toml`, and wiring it properly means teaching `suspend.sh` to
set `rtcWakeup` during the `readyToSuspend` window. Not upstreamable as-is
(upstream has no Kindle), but it is one `if` behind the existing `is_kindle()`
predicate, so it rebases trivially.

### `xbuild.py --test`

`cargo test` needs the same `-L native=…` link paths as a build — bare
`cargo test -p plato-core` fails at `ld: library 'mupdf' not found`. The driver
already computes those paths per profile, so the test verb belongs to it.
One-line change in `run_cargo` plus the flag. Host only; the cross profile
still only knows how to `zigbuild`.

### State

`python3 xbuild.py host --test --package plato-core` → **62 passed, 0 failed**
(unchanged from phase 3 — this phase adds no testable Rust). `python3
xbuild.py kindle` builds `plato` (50 775 KiB) and `plato-harness`, both past
the `e_flags=0x5000200` soft-float ABI gate.

The binary is functionally unchanged for this phase's purpose: the powerd work
is entirely in shell that upstream's `Command::new("scripts/suspend.sh")`
already calls. What was missing at first light was not code but the payload —
the scripts did not exist, and Plato was started with a cwd that would not have
found them anyway.

## Phase A of the PDF work — automatic content-box cropping

Design and measurements live in the `ezkindle` repo (`docs/plato-pdf.md`); this
records only what diverges here. **Phase A is designed to be upstreamable**:
automatic margin detection is a feature upstream plausibly wants, it reuses
`CroppingMargins` unchanged, it is behind a setting, and the whole diff is
additive. Nothing in it touches the EPUB path.

The shape is the one the spike predicted: **one new pure module plus small,
listed additions to four existing files.**

### `crates/core/src/document/layout.rs` (new)

`content_box`, `aggregate_box`, `crop_margin`, `sample_indices` — all pure
functions over slices of `Boundary`, all unit-tested (22 tests, plus 3 more on
committed fixtures). The two decisions that carry the design:

- **Aggregate at the 10th/90th percentile per edge, never the union.** Per-page
  cropping was measured to swing rendered body text by +112% between adjacent
  pages of one paper, because a page whose content happens to occupy one column
  crops to half the width and renders at twice the scale. One bleeding figure
  drags a union out to the paper's edge; the percentile absorbs it. Two tests
  assert exactly that contrast, and a third pins the caveat that a percentile
  *interpolates*, so at a sample of ten one outlier still leaks a few points in
  — which is why the sample defaults to 16 rather than something smaller.
- **Drop rotated lines.** The arXiv stamp is a vertical `arXiv:NNNN.NNNNN` at
  x = 10.9 pt on page 0, and the fixtures confirm it on all three papers: with
  no filter their first page crops from 10.9 instead of from ~70.

### `crates/core/test-data/line-boxes.json` (new, 144 KB)

The fz_stext line and image boxes of `gepa.pdf`, `demo search predict.pdf` and
`wikipedia assist.pdf`, produced by `gen-line-boxes.py`, which walks exactly the
structures `PdfPage::lines()`/`images()` walk. **The PDFs are deliberately not
committed** — they are megabytes each and not ours to redistribute, and the
boxes are the entire input to everything in `layout.rs`. Same reasoning as the
touch capture in phase 3, and the same result: the interesting logic gets a
regression test that costs no dependency and no device.

### Modified files

- **`crates/core/src/document/mupdf_sys.rs`** — `FzTextLine::{wmode, dir}` and
  `FzPoint::{x, y}` become `pub`. Four words; no layout change, `#[repr(C)]`
  is unaffected.

- **`crates/core/src/document/pdf.rs`** — `PdfPage::text_lines()`, a second
  fz_stext walk that keeps each line's `dir` and drops its `TextLocation`.
  A second walk rather than a wider `BoundedText`: `BoundedText` is every
  backend's currency and only layout analysis wants a direction. Plus the two
  trait overrides below.

- **`crates/core/src/document/mod.rs`** — two additive `Document` methods,
  **both with defaults**, so no backend but MuPDF changes at all:
  `text_lines` (defaults to mapping `lines()` with no direction, which is
  exactly what a backend that does not know it should say) and `ink_box`
  (defaults to `None`). MuPDF answers `ink_box` with `PdfPage::boundary_box`,
  which had been present and **never called** since it was written.

- **`crates/core/src/settings/mod.rs`** — `ReaderSettings::auto_crop` (default
  `true`) and `crop_sample_pages` (default 16). The struct already carries
  `#[serde(default)]`, so an existing `Settings.toml` loads unchanged.

- **`crates/core/src/view/reader/mod.rs`** — one free function
  (`auto_crop_margins`) and one guarded block in `Reader::new`. It runs for
  paginated documents only, and only when `cropping_margins` is `None` — a crop
  dragged out in the margin cropper persists through the very same field, so
  automatic detection can never overwrite one.

### The one thing the design got wrong

`docs/plato-pdf.md` §5 says to write the result through the existing
`crop_margins` method, "so `page_offset` remapping and `cache.clear()` are
inherited". **They cannot be.** `crop_margins` starts with
`self.cache.get(&index).unwrap()`, and in `Reader::new` there is no `self` yet,
no cache, and nothing rendered to remap an offset through — going that way is a
panic, not an inheritance. The margins are written into `info.reader` before
construction instead, where `load_pixmap` reads them on the first render; there
is no cache to clear because none has been built. A stored `page_offset` is
reset to zero, because it was measured against an uncropped frame that no
longer exists.

The `crop_margins` path remains exactly right for the case it was written for:
a crop applied to a *running* reader, which is the manual cropper.

### `crates/harness/` — it takes PDFs now

`EpubDocument::new` becomes `plato_core::document::open`, which dispatches
non-EPUB, non-HTML files to MuPDF. Every call the harness already made is on
the `Document` trait, so that is the whole change — and it is what gives the
PDF work a test bed with no fixtures to invent.

One behavioural addition: a paginated document ignores `layout` and rasterises
its page box in points, so a US Letter PDF came out 612x792, a third of the
panel's pixels. Non-reflowable documents are now fit to the panel width, the
same thing `Reader` does under `ZoomMode::FitToWidth`. **EPUB output is
byte-identical**, so the phase-1 host-vs-ARM render comparison still means what
it meant.

### Results

```
python3 xbuild.py host --test --package plato-core     87 passed, 0 failed
  (62 at the end of phase 3; +22 unit, +3 fixture-backed)
PLATO_TEST_PDF=... (the same, +1 integration)          88 passed, 0 failed
python3 xbuild.py kindle                               both binaries, ABI gate passed
  plato          50 770 KiB    e_flags=0x5000200
  plato-harness  48 169 KiB    e_flags=0x5000200
```

Run through the real FFI on four of the sample papers, the aggregate box
reproduces the pymupdf reference numbers to within a point — e.g. `gepa.pdf`
comes out `91.80 / 41.79 / 520.20 / 681.48` against the spike's
`91.8 / 41.0 / 520.2 / 681.7`, across a MuPDF minor version. That agreement is
what says the `dir` field is being read from the right offset.

**Not measured, and it is the one open risk:** sampling 16 pages means 16
fz_stext extractions at open, on a 1 GHz Cortex-A9. If that is seconds, Phase A
needs lazy or background sampling and an uncropped first paint. Nothing was
deployed to the device in this phase.

## Phase B of the PDF work — overlap on turn

Design in `docs/plato-pdf.md` §5 Phase B. **Also upstreamable**: it is one new
setting, defaulted so that turning it to `0` restores today's arithmetic
exactly, and it touches nothing a reflowable document goes through.

Three changes, all in fit-to-width + screen-scroll mode.

### The overlap itself

A screenful repeats the last `scroll-overlap-lines` (default 2) text rows of
the one before it. At the ~4.7 screenfuls per page a two-column paper takes at
a readable zoom (`docs/plato-pdf.md` §4.4) the eye loses its place on every
turn without an anchor.

It is expressed in **rows**, resolved through the page's own fz_stext line
boxes, not as a pixel constant — that is the difference between "about two
lines" and exactly two, and it means the overlap is the same two lines at any
zoom. `overlap_height` in `reader/mod.rs` deliberately takes the same view of
the page `find_cut` takes (same frame containment, same "no line is taller than
a tenth of the frame" filter), because the number is only useful if it lands on
a boundary `find_cut` would also have chosen. A page that answers nothing — a
plate, a scan with no text layer — yields an overlap of zero and a turn
identical to the old one.

Rows, not lines, because a two-column page emits one fz_stext line per column
at nearly the same height; counting lines would halve the overlap on exactly
the documents this is for.

Backwards it is not an offset to subtract but a **shorter screen to fill**: the
previous screenful has to *end* two rows into the current one, so the backward
walk in `go_to_neighbor` accumulates page heights against `previous_span`
instead of the real screen height. Same function, `Forward` from the current
top rather than `Backward` from the cut, and the two are exact inverses — which
is what makes Next-then-Previous land back where it started.

### Continuous scroll by default for paginated documents

`Reader::new` opens a non-reflowable document in `FitToWidth` + `ScrollMode::
Screen` unless stored state says otherwise. Fit-to-page is the wrong default
for a paper: on this panel it is the difference between scrolling and
unreadable type. `continuous-fit-to-width = false` restores the old behaviour,
and reflowable documents are untouched.

### The persistence gates, relaxed

That default needs "nothing stored" to mean "never opened". `quit` therefore
writes a **paginated** document's `zoom_mode`, `scroll_mode` and `page_offset`
whatever they are; previously `zoom_mode` was dropped when it was fit-to-page
and `scroll_mode` unless zoom was fit-to-width, which would have made a
deliberate fit-to-page indistinguishable from a first open — and overridden on
every subsequent one. Reflowable documents keep the old, sparser save path
byte-for-byte. Old `.reading-states` files load unchanged; a PDF in one simply
picks up the new default once.

### The cache cap, and why it was extracted

`update`'s eviction loop (cap 3) is unchanged in behaviour but now calls
`layout::eviction_candidate`. An overlapped screenful can straddle one more
page boundary than an aligned one, so "can eviction throw away a page that is
on screen?" stops being obvious. It can — but only once the screen spans more
pages than the cache holds, which needs pages under half a screen tall, and the
overlap is clamped to half a screen precisely so it cannot produce that on its
own. Both the policy and its limit are now pinned by tests instead of argued
about.

The other invariant worth naming: **a page turn that does not move
`page_offset` is read one frame later as "No next page" and ends the
document.** `next_screen` clamps to `current + 1` so an overlap can never cause
one, and a test walks every combination of cut, overlap and starting offset in
a range to prove it.

### Results

```
python3 xbuild.py host --test --package plato-core     99 passed, 0 failed
  (87 at the end of phase A; +12, all in document/layout.rs)
python3 xbuild.py kindle                               both binaries, ABI gate passed
  plato          50 784 KiB    e_flags=0x5000200
  plato-harness  48 170 KiB    e_flags=0x5000200
```

**Open, and only the device can close it:** whether two rows is the right
number, and whether repeating the same rows across a REAGL turn ghosts visibly.
Nothing was deployed in this phase.
