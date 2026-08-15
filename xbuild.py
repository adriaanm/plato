#!/usr/bin/env python3
"""xbuild.py — the one build driver for the ezkindle fork of Plato.

Replaces the roles of ``thirdparty/download.sh`` + ``thirdparty/build.sh``
(and, for the profiles it supports, the per-library ``build-kobo.sh`` zoo).
Python 3 standard library only; no third-party modules, no shell helpers.

Design
------
* ``SOURCES`` is a declarative table of external C sources: name, version,
  URL and **sha256**.  Nothing is ever fetched unverified.
* ``PROFILES`` names a build target.  Two exist:
    ``host``    macOS/arm64 native, for the SDL2 emulator (phase 0).
    ``kindle``  armv7 soft-float **static musl** via ``zig cc`` (phase 1).
* Each profile declares which system (Homebrew) packages it expects, which
  ``SOURCES`` entries it builds, and the cargo invocation it ends with.
* Every profile keeps its trees apart: ``.xbuild/<profile>/<lib>``.  The two
  profiles build the same MuPDF tarball with different compilers, so they
  must not share an extracted source directory.

Checksums are TOFU — trust on first use.  Each hash below was recorded by
downloading the tarball once and running ``shasum -a 256`` on it.  The point
is not that the first download was trusted, it is that every download from
now on is verified against that recorded value, so a tampered mirror or a
silently re-rolled upstream tarball fails the build instead of being
compiled.  To add a library: put ``sha256=""`` in the table, run once, and
paste the hash the driver prints.

The kindle toolchain
--------------------
Exactly two pinned toolchains, glued by ``cargo-zigbuild``:

* ``zig cc`` / ``zig c++`` for every C and C++ object, targeting
  ``arm-linux-musleabi`` — soft-float **ABI**, which is what the device's
  loader and every Amazon binary use (ezkindle ``docs/toolchain.md``).
  ``-mfloat-abi=softfp -mcpu=cortex_a9`` keeps the soft-float calling
  convention while still emitting VFPv3/NEON instructions: the same choice
  koxtoolchain makes for kindlepw2.
* rustup, pinned by ``rust-toolchain.toml``, target
  ``armv7-unknown-linux-musleabi``, linked through zig, fully static.

Every ARM binary this driver produces goes through the ABI gate before the
build is allowed to succeed — a hard-float slip fails on the device as a
bare "No such file or directory", which is the least debuggable error in
the project.

Usage
-----
    python3 xbuild.py host              # build the C prerequisites + emulator
    python3 xbuild.py host --run        # ... and launch the emulator
    python3 xbuild.py host --clean      # discard .xbuild/<profile>/ and rebuild
    python3 xbuild.py kindle            # cross-build both static armv7 binaries
    python3 xbuild.py kindle --package plato-harness    # just the harness
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import urllib.request
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent
WORK = ROOT / ".xbuild"          # everything this driver creates lives here
CACHE = WORK / "cache"           # verified tarballs
JOBS = str(os.cpu_count() or 4)

# The device, from ezkindle docs/device.md + docs/toolchain.md.
ZIG_TARGET = "arm-linux-musleabi"     # soft-float ABI, static musl
RUST_TARGET = "armv7-unknown-linux-musleabi"
# softfp: soft-float calling convention, VFPv3/NEON instructions.  The i.MX6SL
# has NEON and there is no reason to leave it unused; the *ABI* stays soft,
# which is the part the device cares about.
ZIG_ARCH_FLAGS = ["-mfloat-abi=softfp", "-mcpu=cortex_a9"]

# Packages with no C dependencies at all.  Asking for one of these skips the
# whole native half -- MuPDF and its seven libraries -- which turns a cold
# `xbuild.py kindle --package platonic-recv` from tens of minutes into
# seconds.  Being on this list is a claim about the crate's `[dependencies]`,
# so a crate that grows a `-sys` dependency has to come off it.
#
# `plato-net` is on the list even though ring's build script compiles C: what
# the list is really about is the SOURCES table -- MuPDF and its seven
# libraries -- and ring brings its own C along, built by the same `zig cc`
# cargo-zigbuild already points cc-rs at.
PURE_RUST_PACKAGES = {"platonic-recv", "plato-net"}


# --------------------------------------------------------------------------
# The source table
# --------------------------------------------------------------------------

@dataclass
class Source:
    name: str
    version: str
    url: str
    sha256: str
    # build(src_dir, profile) -> None.  Runs with src_dir already extracted.
    build: Callable[[Path, "Profile"], None] | None = None

    @property
    def tarball(self) -> Path:
        return CACHE / f"{self.name}-{self.version}{_suffix(self.url)}"

    # (`tarball` is also where a .zip asset lands; _suffix keeps the name honest.)

    def src_dir(self, profile: "Profile") -> Path:
        return WORK / profile.name / self.name


def _suffix(url: str) -> str:
    for s in (".tar.gz", ".tgz", ".tar.xz", ".tar.bz2", ".zip"):
        if url.endswith(s):
            return s
    return ".tar.gz"


# --------------------------------------------------------------------------
# Build steps
# --------------------------------------------------------------------------

def build_mupdf(src: Path, profile: "Profile") -> None:
    """Build libmupdf.a + libmupdf-third.a.

    Upstream's thirdparty/mupdf/build-kobo.sh does the same two make calls
    with a cross toolchain, a ``kobo.patch`` that hardcodes it into Makerules,
    and a hand-written shared-link line.  We want static archives and we want
    no patch, so the toolchain and the SYS_*_CFLAGS / SYS_*_LIBS go on the
    make command line instead — the same variables the patch sets.

    ``OS`` is deliberately set to a name MuPDF does not know (``kindle``).
    That is not cosmetic: ``Makerules`` sets ``HAVE_OBJCOPY := yes`` **only**
    for ``OS=Linux``, and with objcopy the embedded fonts get ELF-style
    ``_binary_resources_fonts_..._start`` symbol names.  Without it MuPDF
    falls back to ``scripts/hexdump.sh``, which emits the short
    ``_binary_DroidSansFallback_ttf`` names — and those short names are
    exactly what ``crates/core/src/font/mod.rs`` declares for
    ``target_arch = "arm"``.  Upstream gets this for free via ``OS=kobo``.
    """
    common = [
        "make", "-j", JOBS,
        # Trimmed exactly as upstream does: MuPDF is a codec and an EPUB/PDF
        # engine here, never a viewer.
        "mujs=no", "tesseract=no", "extract=no", "archive=no",
        "brotli=no", "barcode=no", "commercial=no",
        "HAVE_X11=no", "HAVE_GLUT=no", "HAVE_GLFW=no", "HAVE_LIBCRYPTO=no",
        "USE_SYSTEM_LIBS=yes",
        "build=release",
    ]
    if profile.cross:
        common += [
            f"OS={profile.mupdf_os}",
            f"CC={profile.cc}", f"CXX={profile.cxx}", f"AR={profile.ar}",
            f"LD={profile.cc}", f"RANLIB={profile.ranlib}",
            "HAVE_PTHREAD=yes", "SYS_PTHREAD_CFLAGS=", "SYS_PTHREAD_LIBS=",
            # ARCH_HAS_NEON=0 turns off MuPDF's hand-written NEON kernels
            # (deskew_neon.h and friends).  Same wall as libpng's: clang
            # cannot lower <arm_neon.h> vector types for a soft-float-ABI
            # target.  MuPDF exposes the switch as a plain #ifndef, so this
            # costs no patch.  See PATCHES.md.
            "XCFLAGS=" + " ".join(profile.cflags + ["-DARCH_HAS_NEON=0"]),
        ]
        for pkg, var in _MUPDF_SYS_LIBS:
            common.append(f"SYS_{var}_CFLAGS={profile.staged_cflags(pkg)}")
            common.append(f"SYS_{var}_LIBS={profile.staged_libs(pkg)}")
    else:
        for pkg, var in _MUPDF_SYS_LIBS:
            common.append(f"SYS_{var}_CFLAGS={pkgconfig(pkg, '--cflags')}")
            common.append(f"SYS_{var}_LIBS={pkgconfig(pkg, '--libs')}")

    # 'generate' bakes the built-in fonts/CMaps into C sources.  It builds and
    # runs *host* tools, so it must never see the cross compiler -- MuPDF's own
    # Makerules says as much ("Run 'make generate' before doing the cross
    # compile").  Hence a plain, unqualified make here in both profiles.
    run(["make", "-j", JOBS, "generate"], cwd=src)
    run(common + ["libs"], cwd=src)

    out = src / "build" / "release"
    for lib in ("libmupdf.a", "libmupdf-third.a"):
        if not (out / lib).exists():
            die(f"mupdf build produced no {lib}")
    profile.link_search.append(out)


# MuPDF's system-library knobs, in (pkg-config name, Makerules variable) form.
_MUPDF_SYS_LIBS = (
    ("freetype2", "FREETYPE"), ("harfbuzz", "HARFBUZZ"),
    ("gumbo", "GUMBO"), ("jbig2dec", "JBIG2DEC"),
    ("libjpeg", "LIBJPEG"), ("libopenjp2", "OPENJPEG"),
    ("zlib", "ZLIB"),
)


def build_mupdf_wrapper(profile: "Profile") -> None:
    """Compile Plato's own C shim against the MuPDF headers we just built.

    Upstream does this in mupdf_wrapper/build.sh, which hardcodes
    ../thirdparty/mupdf/include.  We keep that script untouched (it is what
    a rebase onto upstream expects) and compile the one .c file here so the
    include path can point at .xbuild/<profile>/mupdf instead.
    """
    src = ROOT / "mupdf_wrapper" / "mupdf_wrapper.c"
    out = ROOT / "target" / "mupdf_wrapper" / profile.target_os
    out.mkdir(parents=True, exist_ok=True)
    obj, lib = out / "mupdf_wrapper.o", out / "libmupdf_wrapper.a"
    run([profile.cc, *profile.cflags, "-O2", "-fPIC",
         f"-I{SOURCES['mupdf'].src_dir(profile) / 'include'}",
         "-c", str(src), "-o", str(obj)])
    lib.unlink(missing_ok=True)
    run([profile.ar, "-rcs", str(lib), str(obj)])
    profile.link_search.append(out)


# ---- the cross C stack ----------------------------------------------------
#
# Each of these replaces one thirdparty/<lib>/build-kobo.sh.  They install
# into a single staging prefix so that MuPDF, the Rust link line and each
# other all see one -I/-L pair.

def build_zlib(src: Path, profile: "Profile") -> None:
    """zlib's configure is not autoconf: no --host, and it decides how to make
    an archive from ``uname``.  Left alone on macOS it picks Apple's
    ``libtool``, which cannot put ARM ELF objects in an archive
    ("adler32.o is not an object file").  ``--uname=Linux`` is the documented
    override and puts it back on ``$AR rc``."""
    env = profile.autotools_env()
    env["CHOST"] = ZIG_TARGET
    run(["./configure", "--static", "--uname=Linux",
         f"--prefix={profile.prefix}"], cwd=src, env=env)
    run(["make", "-j", JOBS, "install"], cwd=src, env=env)


def autotools(*extra: str, env_extra: dict | None = None):
    """A build step that runs ./configure --host=... && make install."""
    def step(src: Path, profile: "Profile") -> None:
        env = profile.autotools_env()
        if env_extra:
            env.update({k: v.format(prefix=profile.prefix) for k, v in env_extra.items()})
        run(["./configure",
             f"--host={ZIG_TARGET}",
             f"--prefix={profile.prefix}",
             "--enable-static", "--disable-shared",
             *extra], cwd=src, env=env)
        run(["make", "-j", JOBS], cwd=src, env=env)
        run(["make", "install"], cwd=src, env=env)
    return step


def build_openjpeg(src: Path, profile: "Profile") -> None:
    """OpenJPEG is cmake-only.  Point cmake at the zig wrappers.

    MuPDF wants ``-lopenjp2`` and the headers on its include path; openjpeg
    installs them into ``include/openjpeg-2.5/``, so they get copied flat
    afterwards (MuPDF includes <openjpeg.h>).
    """
    build = src / "build"
    shutil.rmtree(build, ignore_errors=True)
    build.mkdir()
    run(["cmake", "..",
         "-DCMAKE_BUILD_TYPE=Release",
         "-DCMAKE_SYSTEM_NAME=Linux",
         "-DCMAKE_SYSTEM_PROCESSOR=arm",
         f"-DCMAKE_INSTALL_PREFIX={profile.prefix}",
         f"-DCMAKE_C_COMPILER={profile.cc}",
         f"-DCMAKE_AR={profile.ar}",
         f"-DCMAKE_RANLIB={profile.ranlib}",
         "-DCMAKE_C_FLAGS=" + " ".join(profile.cflags),
         "-DBUILD_CODEC=OFF", "-DBUILD_SHARED_LIBS=OFF",
         "-DBUILD_STATIC_LIBS=ON",
         f"-DZLIB_INCLUDE_DIR={profile.prefix}/include",
         f"-DZLIB_LIBRARY={profile.prefix}/lib/libz.a",
         ], cwd=build)
    run(["make", "-j", JOBS, "install"], cwd=build)
    inc = profile.prefix / "include"
    for d in inc.glob("openjpeg-*"):
        for header in d.glob("*.h"):
            shutil.copy2(header, inc / header.name)


def build_gumbo(src: Path, profile: "Profile") -> None:
    """gumbo-parser has no ``configure`` in the GitHub archive — only
    ``configure.ac``, so upstream runs ``autogen.sh``.  It is nine C99 files
    with no generated headers, so compiling them directly is both simpler and
    one fewer host tool (no autoreconf/libtool in the dependency set)."""
    objs = []
    obj_dir = src / "obj"
    obj_dir.mkdir(exist_ok=True)
    for c in sorted((src / "src").glob("*.c")):
        obj = obj_dir / (c.stem + ".o")
        run([profile.cc, *profile.cflags, "-O2", "-std=c99", "-fPIC",
             f"-I{src / 'src'}", "-c", str(c), "-o", str(obj)])
        objs.append(str(obj))
    lib = profile.prefix / "lib" / "libgumbo.a"
    lib.parent.mkdir(parents=True, exist_ok=True)
    lib.unlink(missing_ok=True)
    run([profile.ar, "-rcs", str(lib), *objs])
    run([profile.ranlib, str(lib)])
    for h in ("gumbo.h", "tag_enum.h"):
        p = src / "src" / h
        if p.exists():
            shutil.copy2(p, profile.prefix / "include" / h)


def build_c23_compat(profile: "Profile") -> None:
    """musl has no fminimum_num*/fmaximum_num*; rustc 1.97 emits calls to
    them for f32::min / f32::max.  See csupport/c23_math_compat.c."""
    src = ROOT / "csupport" / "c23_math_compat.c"
    obj = WORK / profile.name / "c23_math_compat.o"
    lib = profile.prefix / "lib" / "libc23compat.a"
    run([profile.cc, *profile.cflags, "-O2", "-std=c11",
         "-c", str(src), "-o", str(obj)])
    lib.unlink(missing_ok=True)
    run([profile.ar, "-rcs", str(lib), str(obj)])
    run([profile.ranlib, str(lib)])


def build_harfbuzz(src: Path, profile: "Profile") -> None:
    """One ``zig c++`` invocation, no meson, no ninja.

    HarfBuzz ships ``src/harfbuzz.cc``, an amalgam that #includes every other
    .cc in the library.  Upstream Plato drives meson with a cross file; the
    amalgam removes meson, ninja and a cross file from our tool list for the
    cost of one command.  ``HAVE_FREETYPE`` is what pulls in ``hb-ft.cc``,
    which is the only part Plato's FFI actually needs
    (``hb_ft_font_create``).
    """
    obj = src / "harfbuzz.o"
    run([profile.cxx, *profile.cflags, "-O2", "-fPIC",
         "-std=c++17", "-fno-exceptions", "-fno-rtti", "-fno-threadsafe-statics",
         "-DHAVE_FREETYPE=1", "-DHB_NO_MT",
         f"-I{profile.prefix / 'include' / 'freetype2'}",
         f"-I{profile.prefix / 'include'}",
         f"-I{src / 'src'}",
         "-c", str(src / "src" / "harfbuzz.cc"), "-o", str(obj)])
    lib = profile.prefix / "lib" / "libharfbuzz.a"
    lib.unlink(missing_ok=True)
    run([profile.ar, "-rcs", str(lib), str(obj)])
    run([profile.ranlib, str(lib)])
    inc = profile.prefix / "include" / "harfbuzz"
    inc.mkdir(parents=True, exist_ok=True)
    for h in sorted((src / "src").glob("hb*.h")):
        shutil.copy2(h, inc / h.name)


# --------------------------------------------------------------------------
# Profiles
# --------------------------------------------------------------------------

@dataclass
class Profile:
    name: str
    cc: str = "cc"
    cxx: str = "c++"
    ar: str = "ar"
    ranlib: str = "ranlib"
    cross: bool = False
    target_os: str = ""            # target/mupdf_wrapper/<target_os>
    mupdf_os: str = ""             # MuPDF's OS= (see build_mupdf)
    brew_packages: tuple[str, ...] = ()
    pkgconfig_libs: tuple[str, ...] = ()
    sources: tuple[str, ...] = ()
    # A profile may build more than one binary. The kindle profile builds two:
    # the device binary and the headless render harness, which is the thing
    # that can actually be run under qemu.
    cargo_packages: tuple[str, ...] = ("emulator",)
    cargo_target: str = ""
    cargo_features: tuple[str, ...] = ()      # djvu deliberately absent
    extra_link_libs: tuple[str, ...] = ()
    extra_rustflags: tuple[str, ...] = ()
    cflags: list[str] = field(default_factory=list)
    link_search: list[Path] = field(default_factory=list)

    @property
    def prefix(self) -> Path:
        return WORK / self.name / "prefix"

    def staged_cflags(self, pkg: str) -> str:
        # freetype and harfbuzz both put their headers in a subdirectory and
        # both are #included flat by MuPDF ("ft2build.h", "hb.h").
        subdir = {"freetype2": "freetype2", "harfbuzz": "harfbuzz"}.get(pkg)
        extra = f" -I{self.prefix}/include/{subdir}" if subdir else ""
        return f"-I{self.prefix}/include{extra}"

    def staged_libs(self, pkg: str) -> str:
        return f"-L{self.prefix}/lib -l{_PKG_LINK_NAME[pkg]}"

    def autotools_env(self) -> dict:
        env = dict(os.environ)
        env.update(
            CC=self.cc, CXX=self.cxx, AR=self.ar, RANLIB=self.ranlib,
            CFLAGS=" ".join(self.cflags + ["-O2"]),
            CXXFLAGS=" ".join(self.cflags + ["-O2"]),
            CPPFLAGS=f"-I{self.prefix}/include",
            LDFLAGS=f"-L{self.prefix}/lib",
            PKG_CONFIG_PATH=f"{self.prefix}/lib/pkgconfig",
            PKG_CONFIG_LIBDIR=f"{self.prefix}/lib/pkgconfig",
        )
        return env


_PKG_LINK_NAME = {
    "freetype2": "freetype", "harfbuzz": "harfbuzz", "gumbo": "gumbo",
    "jbig2dec": "jbig2dec", "libjpeg": "jpeg", "libopenjp2": "openjp2",
    "zlib": "z",
}


HOST = Profile(
    name="host",
    target_os=platform.system(),
    brew_packages=("sdl2", "freetype", "harfbuzz", "jpeg-turbo",
                   "openjpeg", "jbig2dec", "gumbo-parser"),
    pkgconfig_libs=("sdl2", "freetype2", "harfbuzz", "libjpeg",
                    "libopenjp2", "jbig2dec", "gumbo", "zlib"),
    sources=("mupdf",),
    cargo_packages=("emulator",),
    # libmupdf.a's own undefined symbols.  Plato's #[link] attributes only
    # name mupdf / mupdf_wrapper / freetype / harfbuzz, so the rest of
    # MuPDF's dependency set has to be added on the link line.
    extra_link_libs=("mupdf-third", "gumbo", "jbig2dec", "jpeg",
                     "openjp2", "z"),
)

KINDLE = Profile(
    name="kindle",
    cc=str(WORK / "kindle" / "bin" / "zig-cc"),
    cxx=str(WORK / "kindle" / "bin" / "zig-cxx"),
    ar=str(WORK / "kindle" / "bin" / "zig-ar"),
    ranlib=str(WORK / "kindle" / "bin" / "zig-ranlib"),
    cross=True,
    target_os="Kindle",
    mupdf_os="kindle",
    # Order matters: each entry is built against the ones before it.
    sources=("zlib", "libpng", "libjpeg", "openjpeg", "jbig2dec",
             "freetype2", "harfbuzz", "gumbo", "mupdf"),
    # `plato` is the real device binary (phase 2's framebuffer/device backend);
    # `plato-harness` is the headless EPUB -> PNG smoke test, and the only one
    # of the two that runs under qemu-user. Both are gated on the ABI check.
    cargo_packages=("plato", "plato-harness"),
    cargo_target=RUST_TARGET,
    # Nothing here: crates/core/build.rs already names the whole set for this
    # target, and naming them twice only makes the link line harder to read.
    extra_link_libs=(),
    extra_rustflags=("-C", "target-feature=+crt-static"),
)

PROFILES = {"host": HOST, "kindle": KINDLE}

SOURCES = {
    # sha256 recorded 2026-08-09 (TOFU, see module docstring).  Every URL is
    # https; upstream's download.sh fetches libjpeg and djvulibre over plain
    # http, which is one of the reasons it is not used here.
    "zlib": Source(
        name="zlib", version="1.3.1",
        # Upstream Plato uses zlib.net, which serves an HTML stub for anything
        # but the current release.  The GitHub release tarball is the same
        # file (this is the widely published 1.3.1 hash).
        url="https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz",
        sha256="9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23",
        build=build_zlib,
    ),
    "libpng": Source(
        name="libpng", version="1.6.53",
        url="https://download.sourceforge.net/libpng/libpng-1.6.53.tar.gz",
        sha256="da0b045cbb1d06a8fc9696f9441359f70645f280ff24ae453ccb7c722353654f",
        # --enable-arm-neon=no is not optional: clang cannot compile
        # hand-written <arm_neon.h> intrinsics for a soft-float *ABI* target
        # ("fatal error in backend: Do not know how to split this operator's
        # operand" under softfp; arm_neon.h isn't even available under plain
        # soft).  Auto-vectorised NEON is unaffected, and libpng is only here
        # for freetype's PNG-in-font glyphs, so the filter fast paths are
        # worth nothing to us.  See PATCHES.md.
        build=autotools("--enable-arm-neon=no"),
    ),
    "libjpeg": Source(
        name="libjpeg", version="9f",
        url="https://www.ijg.org/files/jpegsrc.v9f.tar.gz",
        sha256="04705c110cb2469caa79fb71fba3d7bf834914706e9641a4589485c1f832565b",
        build=autotools(),
    ),
    "openjpeg": Source(
        name="openjpeg", version="2.5.4",
        url="https://github.com/uclouvain/openjpeg/archive/v2.5.4.tar.gz",
        sha256="a695fbe19c0165f295a8531b1e4e855cd94d0875d2f88ec4b61080677e27188a",
        build=build_openjpeg,
    ),
    "jbig2dec": Source(
        name="jbig2dec", version="0.20",
        url="https://github.com/ArtifexSoftware/jbig2dec/releases/download/0.20/jbig2dec-0.20.tar.gz",
        sha256="7b63ff6470289547e7a3a0f145cb8ea6c2afffdd65645b7d87d3b7febc96fb3a",
        build=autotools("--without-libpng", "--disable-tests"),
    ),
    "freetype2": Source(
        name="freetype2", version="2.14.1",
        url="https://download.savannah.gnu.org/releases/freetype/freetype-2.14.1.tar.gz",
        sha256="174d9e53402e1bf9ec7277e22ec199ba3e55a6be2c0740cb18c0ee9850fc8c34",
        # --with-harfbuzz=no breaks the freetype<->harfbuzz cycle: freetype is
        # built first and only uses harfbuzz for autohinting complex scripts.
        build=autotools("--with-zlib=yes", "--with-png=yes", "--with-bzip2=no",
                        "--with-harfbuzz=no", "--with-brotli=no"),
    ),
    "harfbuzz": Source(
        name="harfbuzz", version="12.3.0",
        url="https://github.com/harfbuzz/harfbuzz/archive/12.3.0.tar.gz",
        sha256="e93af4816128fc0a02d2e84106fdfe36a3fde01086b723be8f0656a65562ca9e",
        build=build_harfbuzz,
    ),
    "gumbo": Source(
        name="gumbo", version="0.10.1",
        url="https://github.com/google/gumbo-parser/archive/v0.10.1.tar.gz",
        sha256="28463053d44a5dfbc4b77bcf49c8cee119338ffa636cc17fc3378421d714efad",
        build=build_gumbo,
    ),
    "mupdf": Source(
        name="mupdf",
        version="1.27.0",
        url="https://casper.mupdf.com/downloads/archive/mupdf-1.27.0-source.tar.gz",
        sha256="ae2442416de499182d37a526c6fa2bacc7a3bed5a888d113ca04844484dfe7c6",
        build=build_mupdf,
    ),
}

# Data-only assets pulled out of upstream's own GitHub release zip.  Upstream
# gets these via ./download.sh, which also serves prebuilt ARM shared objects
# from the maintainer's server -- a path we never use.  Here the zip is
# sha256-pinned and *only* the named data directories are extracted, so no
# binary from it ever reaches the build.
RELEASE_ZIP = Source(
    name="plato-release",
    version="0.9.45",
    url="https://github.com/baskerville/plato/releases/download/0.9.45/plato-0.9.45.zip",
    sha256="d89b828ff02ae2c835e14476be58b5475c612310a101dae00ad367935cece8cb",
)
RELEASE_ASSETS = ("hyphenation-patterns",)


def fetch_release_assets() -> None:
    """Unpack the hyphenation patterns; without them Plato does not hyphenate.

    They are not in the git tree, and on a device-class 600px column their
    absence is very visible -- which matters, because phase 0 is a judgement
    about typography.
    """
    if all((ROOT / a).is_dir() for a in RELEASE_ASSETS):
        return
    fetch(RELEASE_ZIP)
    log(f"extracting {', '.join(RELEASE_ASSETS)} from the upstream release zip")
    with zipfile.ZipFile(RELEASE_ZIP.tarball) as zf:
        for info in zf.infolist():
            top = Path(info.filename).parts[0]
            if top in RELEASE_ASSETS and not info.is_dir():
                zf.extract(info, ROOT)


# --------------------------------------------------------------------------
# Plumbing
# --------------------------------------------------------------------------

def die(msg: str) -> "None":
    print(f"xbuild: {msg}", file=sys.stderr)
    raise SystemExit(1)


def log(msg: str) -> None:
    print(f"==> {msg}", flush=True)


def run(cmd: list[str], cwd: Path | None = None, env: dict | None = None) -> None:
    proc = subprocess.run(cmd, cwd=cwd, env=env)
    if proc.returncode != 0:
        die(f"command failed ({proc.returncode}): {' '.join(cmd)}")


def pkgconfig(pkg: str, what: str) -> str:
    out = subprocess.run(["pkg-config", what, pkg],
                         capture_output=True, text=True)
    if out.returncode != 0:
        die(f"pkg-config has no '{pkg}'. Install it (see the profile's "
            f"brew_packages) and retry.")
    return out.stdout.strip()


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch(source: Source) -> None:
    CACHE.mkdir(parents=True, exist_ok=True)
    tarball = source.tarball
    if not tarball.exists():
        log(f"downloading {source.name} {source.version}")
        tmp = tarball.with_suffix(tarball.suffix + ".part")
        with urllib.request.urlopen(source.url) as r, tmp.open("wb") as f:
            shutil.copyfileobj(r, f)
        tmp.replace(tarball)
    got = sha256_of(tarball)
    if not source.sha256:
        die(f"{source.name}: no sha256 recorded. Downloaded hash is:\n"
            f"    {got}\nPaste it into the SOURCES table (TOFU).")
    if got != source.sha256:
        tarball.unlink()
        die(f"{source.name}: sha256 mismatch\n  expected {source.sha256}\n"
            f"  got      {got}\nThe cached copy was deleted; do not retry "
            f"blindly.")


def extract(source: Source, profile: Profile) -> None:
    src_dir = source.src_dir(profile)
    if (src_dir / ".xbuild-extracted").exists():
        return
    log(f"extracting {source.name} {source.version}")
    shutil.rmtree(src_dir, ignore_errors=True)
    src_dir.mkdir(parents=True)
    with tarfile.open(source.tarball) as tf:
        members = []
        for m in tf.getmembers():
            parts = Path(m.name).parts
            if len(parts) < 2:
                continue
            m.name = str(Path(*parts[1:]))     # --strip-components 1
            members.append(m)
        tf.extractall(src_dir, members=members, filter="tar")
    (src_dir / ".xbuild-extracted").touch()


def check_system_deps(profile: Profile) -> None:
    missing = [p for p in profile.pkgconfig_libs
               if subprocess.run(["pkg-config", "--exists", p]).returncode != 0]
    if missing:
        die(f"missing pkg-config packages: {', '.join(missing)}\n"
            f"    brew install {' '.join(profile.brew_packages)}")


def cargo_env(profile: Profile) -> dict:
    env = dict(os.environ)
    flags = list(profile.extra_rustflags)
    for d in profile.link_search:
        flags += ["-L", f"native={d}"]
    for d in {Path(w[2:]) for lib in profile.pkgconfig_libs
              for w in pkgconfig(lib, "--libs").split() if w.startswith("-L")}:
        flags += ["-L", f"native={d}"]
    for lib in profile.extra_link_libs:
        flags += ["-l", lib]
    env["RUSTFLAGS"] = " ".join(flags + [env.get("RUSTFLAGS", "")]).strip()
    return env


def build_sources(profile: Profile) -> None:
    for name in profile.sources:
        source = SOURCES[name]
        fetch(source)
        extract(source, profile)
        src_dir = source.src_dir(profile)
        if source.build and not (src_dir / ".xbuild-built").exists():
            log(f"building {source.name} [{profile.name}]")
            source.build(src_dir, profile)
            (src_dir / ".xbuild-built").touch()
        elif source.build:
            # Already built: re-declare whatever the step would have added to
            # the link path.  Only MuPDF has an in-tree output directory; the
            # rest install into the staging prefix.
            if name == "mupdf":
                profile.link_search.append(src_dir / "build" / "release")


def run_cargo(profile: Profile, args) -> None:
    env = cargo_env(profile)
    packages = [a for pkg in profile.cargo_packages for a in ("-p", pkg)]
    if profile.cross:
        cargo, rustc = rustup_tools()
        env["RUSTC"] = rustc
        env["PATH"] = f"{Path(rustc).parent}:{env['PATH']}"
        cmd = [cargo, "zigbuild", *packages,
               "--target", profile.cargo_target, "--release"]
    else:
        verb = "run" if args.run else "test" if args.test else "build"
        # Release here too, and not for speed: a debug build of this workspace
        # is several gigabytes of debuginfo, and a `cargo test` that quietly
        # creates a second, debug copy of everything next to the release one is
        # how this machine runs out of disk mid-link -- which surfaces as a bare
        # "linker command failed", not as "no space left".  The cross profile
        # above has always been release; this makes the host match.
        cmd = ["cargo", verb, *packages, "--release"]
    if profile.cargo_features:
        cmd += ["--features", ",".join(profile.cargo_features)]
    cmd += args.cargo
    log(" ".join(cmd))
    run(cmd, cwd=ROOT, env=env)


def build_host(profile: Profile, args) -> None:
    if platform.system() != "Darwin":
        die("the 'host' profile is macOS-only; add a branch here for Linux.")
    check_system_deps(profile)
    fetch_release_assets()
    build_sources(profile)
    log("building mupdf_wrapper")
    build_mupdf_wrapper(profile)
    run_cargo(profile, args)


# --------------------------------------------------------------------------
# The kindle profile
# --------------------------------------------------------------------------

_ZIG_WRAPPERS = {
    "zig-cc":     ["cc", "-target", ZIG_TARGET, *ZIG_ARCH_FLAGS],
    "zig-cxx":    ["c++", "-target", ZIG_TARGET, *ZIG_ARCH_FLAGS],
    "zig-ar":     ["ar"],
    "zig-ranlib": ["ranlib"],
}


def write_zig_wrappers(profile: Profile) -> None:
    """configure, cmake and make all want ``$CC`` to be one word.

    ``zig cc -target ...`` is five, and quoting it survives none of those
    three.  Four two-line shell wrappers is the whole answer, and it also
    means the target triple and the -mcpu/-mfloat-abi choice are written
    down in exactly one place.
    """
    zig = shutil.which("zig") or die("zig is not on PATH")
    bindir = WORK / profile.name / "bin"
    bindir.mkdir(parents=True, exist_ok=True)
    for name, argv in _ZIG_WRAPPERS.items():
        p = bindir / name
        p.write_text("#!/bin/sh\nexec {} {} \"$@\"\n"
                     .format(zig, " ".join(argv)))
        p.chmod(p.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def check_arm_abi(path: Path) -> None:
    """The hard build gate.  A hard-float binary fails on the device as a bare
    "No such file or directory" -- the missing ld-linux-armhf.so.3 loader --
    which reads as "the file isn't there", not "the ABI is wrong".  Cheapest
    possible place to catch it is here.

    Same check as ezkindle's scripts/check-arm-abi.py, reimplemented so this
    fork has no path dependency on a sibling checkout; if that checkout *is*
    next door, it is run too, so the two can never silently disagree.
    """
    with path.open("rb") as f:
        if f.read(4) != b"\x7fELF":
            die(f"{path}: not an ELF")
        f.seek(0x12)
        machine = int.from_bytes(f.read(2), "little")
        f.seek(0x24)
        flags = int.from_bytes(f.read(4), "little")
    if machine != 0x28:
        die(f"{path}: e_machine=0x{machine:x}, not ARM")
    if flags & 0x400:                      # EF_ARM_ABI_FLOAT_HARD
        die(f"{path}: e_flags=0x{flags:x} -- HARD-float, WRONG for this device")
    log(f"ABI gate: {path.name}: e_flags=0x{flags:x} soft-float, correct")

    sibling = ROOT.parent / "ezkindle" / "scripts" / "check-arm-abi.py"
    if sibling.exists():
        run([sys.executable, str(sibling), str(path)])


def rustup_tools() -> tuple[str, str]:
    """Resolve (cargo, rustc) from rustup, both by absolute path.

    This Mac has Homebrew's rust first on PATH and other projects depend on
    it, so nothing here reorders the global PATH or changes rustup's default
    toolchain.  ``rustup which`` is asked from the repository root, so
    rust-toolchain.toml decides -- which is what finally makes that pin mean
    something (see PATCHES.md).

    **rustc has to be named explicitly.**  Homebrew's rustup shim is a bash
    wrapper, and a cargo reached through it still finds ``rustc`` by PATH --
    i.e. Homebrew's rustc, which has no cross targets installed.  The symptom
    is a very convincing lie: "can't find crate for `core` ... the target may
    not be installed", for a target that *is* installed.
    """
    rustup = shutil.which("rustup")
    if not rustup:
        die("rustup is not on PATH (brew install rustup), so the "
            f"{RUST_TARGET} std cannot be found")
    def which(tool: str) -> str:
        out = subprocess.run([rustup, "which", tool], cwd=ROOT,
                             capture_output=True, text=True)
        if out.returncode != 0:
            die(f"rustup which {tool} failed:\n{out.stderr.strip()}")
        return out.stdout.strip()
    return which("cargo"), which("rustc")


def build_kindle(profile: Profile, args) -> None:
    if not shutil.which("cargo-zigbuild"):
        die("cargo-zigbuild is not on PATH (brew install cargo-zigbuild)")
    write_zig_wrappers(profile)
    if set(profile.cargo_packages) <= PURE_RUST_PACKAGES:
        log(f"pure-Rust packages ({', '.join(profile.cargo_packages)}): "
            "skipping the native sources")
        run_cargo(profile, args)
        report_binaries(profile)
        return
    (profile.prefix / "include").mkdir(parents=True, exist_ok=True)
    (profile.prefix / "lib").mkdir(parents=True, exist_ok=True)
    profile.cflags = list(ZIG_ARCH_FLAGS)
    fetch_release_assets()
    build_sources(profile)
    log("building c23 math compat shim")
    build_c23_compat(profile)
    log("building mupdf_wrapper")
    build_mupdf_wrapper(profile)
    profile.link_search.append(profile.prefix / "lib")
    run_cargo(profile, args)
    report_binaries(profile)


def package_binaries(pkg: str) -> list[str]:
    """The ``[[bin]]`` names a package produces, asked of cargo rather than
    assumed from the package name.  They coincide for ``plato`` and
    ``plato-harness``; they do not for ``plato-net`` (``net-smoke``) or
    ``foldersync`` (two binaries), and a wrong guess here reads as "expected a
    binary at ..." after a build that in fact succeeded."""
    cargo = shutil.which("cargo") or die("cargo is not on PATH")
    out = subprocess.run([cargo, "metadata", "--no-deps", "--format-version", "1"],
                         cwd=ROOT, capture_output=True, text=True, check=True).stdout
    for package in json.loads(out)["packages"]:
        if package["name"] == pkg:
            return [t["name"] for t in package["targets"] if "bin" in t["kind"]]
    die(f"no package named {pkg} in the workspace")


def report_binaries(profile: Profile) -> None:
    """The ABI gate, on every binary the profile produced.  Nothing leaves this
    driver unchecked -- a hard-float binary fails on the device as a bare "No
    such file or directory"."""
    for pkg in profile.cargo_packages:
        for name in package_binaries(pkg):
            out = ROOT / "target" / profile.cargo_target / "release" / name
            if not out.exists():
                die(f"expected a binary at {out}")
            check_arm_abi(out)
            log(f"{out}  ({out.stat().st_size // 1024} KiB)")


BUILDERS = {"host": build_host, "kindle": build_kindle}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("profile", choices=sorted(PROFILES))
    p.add_argument("--run", action="store_true",
                   help="cargo run instead of cargo build (host only)")
    p.add_argument("--test", action="store_true",
                   help="cargo test instead of cargo build (host only). The "
                        "unit tests need the same link paths as a build, so "
                        "they have to go through this driver, not bare cargo.")
    p.add_argument("--package", metavar="NAME", action="append",
                   help="cargo package to build instead of the profile's "
                        "defaults (host: emulator; kindle: plato and "
                        "plato-harness). Repeatable.")
    p.add_argument("--clean", action="store_true",
                   help="remove .xbuild/<profile>/ (keeping the verified tarballs)")
    p.add_argument("cargo", nargs="*",
                   help="extra arguments passed through to cargo")
    args = p.parse_args()

    profile = PROFILES[args.profile]
    if args.package:
        profile.cargo_packages = tuple(args.package)
    if args.clean:
        shutil.rmtree(WORK / profile.name, ignore_errors=True)

    BUILDERS[args.profile](profile, args)


if __name__ == "__main__":
    main()
