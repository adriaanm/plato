#!/usr/bin/env python3
"""xbuild.py — the one build driver for the ezkindle fork of Plato.

Replaces the roles of ``thirdparty/download.sh`` + ``thirdparty/build.sh``
(and, for the profiles it supports, the per-library ``build-kobo.sh`` zoo).
Python 3 standard library only; no third-party modules, no shell helpers.

Design
------
* ``SOURCES`` is a declarative table of external C sources: name, version,
  URL and **sha256**.  Nothing is ever fetched unverified.
* ``PROFILES`` names a build target.  Two exist today:
    ``host``    macOS/arm64 native, for the SDL2 emulator (phase 0).
    ``kindle``  armv7 soft-float static via zig — phase 1, stubbed out.
* Each profile declares which system (Homebrew) packages it expects, which
  ``SOURCES`` entries it builds, and the cargo invocation it ends with.

Checksums are TOFU — trust on first use.  Each hash below was recorded by
downloading the tarball once and running ``shasum -a 256`` on it.  The point
is not that the first download was trusted, it is that every download from
now on is verified against that recorded value, so a tampered mirror or a
silently re-rolled upstream tarball fails the build instead of being
compiled.  To add a library: put ``sha256=""`` in the table, run once, and
paste the hash the driver prints.

Usage
-----
    python3 xbuild.py host              # build the C prerequisites + emulator
    python3 xbuild.py host --run        # ... and launch the emulator
    python3 xbuild.py host --clean      # discard .xbuild/ and rebuild
    python3 xbuild.py kindle            # phase 1: not implemented yet
"""

from __future__ import annotations

import argparse
import hashlib
import os
import platform
import shutil
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

    @property
    def src_dir(self) -> Path:
        return WORK / self.name


def _suffix(url: str) -> str:
    for s in (".tar.gz", ".tgz", ".tar.xz", ".tar.bz2", ".zip"):
        if url.endswith(s):
            return s
    return ".tar.gz"


# --------------------------------------------------------------------------
# Build steps
# --------------------------------------------------------------------------

def build_mupdf(src: Path, profile: "Profile") -> None:
    """Build libmupdf.a + libmupdf-third.a against the system libraries.

    Upstream's thirdparty/mupdf/build-kobo.sh does the same two make calls
    with a cross toolchain and a hand-written shared-link line.  On the host
    we want static archives and the Homebrew copies of freetype/harfbuzz/…,
    so that exactly one copy of each library ends up in the binary (Plato's
    Rust also links freetype and harfbuzz directly).

    MuPDF's Makerules has no pkg-config path for these on macOS, so the
    SYS_*_CFLAGS / SYS_*_LIBS are passed in explicitly — the same variables
    upstream's kobo.patch sets, pointed at Homebrew instead.
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
    for pkg, var in (("freetype2", "FREETYPE"), ("harfbuzz", "HARFBUZZ"),
                     ("gumbo", "GUMBO"), ("jbig2dec", "JBIG2DEC"),
                     ("libjpeg", "LIBJPEG"), ("libopenjp2", "OPENJPEG"),
                     ("zlib", "ZLIB")):
        common.append(f"SYS_{var}_CFLAGS={pkgconfig(pkg, '--cflags')}")
        common.append(f"SYS_{var}_LIBS={pkgconfig(pkg, '--libs')}")

    # 'generate' bakes the built-in fonts/CMaps into C sources; it must run
    # with the *host* compiler, which on the host profile it already is.
    run(["make", "-j", JOBS, "generate"], cwd=src)
    run(common + ["libs"], cwd=src)

    out = src / "build" / "release"
    for lib in ("libmupdf.a", "libmupdf-third.a"):
        if not (out / lib).exists():
            die(f"mupdf build produced no {lib}")
    profile.link_search.append(out)


def build_mupdf_wrapper(profile: "Profile") -> None:
    """Compile Plato's own C shim against the MuPDF headers we just built.

    Upstream does this in mupdf_wrapper/build.sh, which hardcodes
    ../thirdparty/mupdf/include.  We keep that script untouched (it is what
    a rebase onto upstream expects) and compile the one .c file here so the
    include path can point at .xbuild/mupdf instead.
    """
    src = ROOT / "mupdf_wrapper" / "mupdf_wrapper.c"
    out = ROOT / "target" / "mupdf_wrapper" / platform.system()
    out.mkdir(parents=True, exist_ok=True)
    obj, lib = out / "mupdf_wrapper.o", out / "libmupdf_wrapper.a"
    run([profile.cc, "-O2", "-fPIC",
         f"-I{WORK / 'mupdf' / 'include'}",
         "-c", str(src), "-o", str(obj)])
    lib.unlink(missing_ok=True)
    run([profile.ar, "-rcs", str(lib), str(obj)])
    profile.link_search.append(out)


# --------------------------------------------------------------------------
# Profiles
# --------------------------------------------------------------------------

@dataclass
class Profile:
    name: str
    cc: str = "cc"
    ar: str = "ar"
    brew_packages: tuple[str, ...] = ()
    pkgconfig_libs: tuple[str, ...] = ()
    sources: tuple[str, ...] = ()
    cargo_package: str = "emulator"
    cargo_features: tuple[str, ...] = ()      # djvu deliberately absent
    extra_link_libs: tuple[str, ...] = ()
    link_search: list[Path] = field(default_factory=list)


HOST = Profile(
    name="host",
    brew_packages=("sdl2", "freetype", "harfbuzz", "jpeg-turbo",
                   "openjpeg", "jbig2dec", "gumbo-parser"),
    pkgconfig_libs=("sdl2", "freetype2", "harfbuzz", "libjpeg",
                    "libopenjp2", "jbig2dec", "gumbo", "zlib"),
    sources=("mupdf",),
    cargo_package="emulator",
    # libmupdf.a's own undefined symbols.  Plato's #[link] attributes only
    # name mupdf / mupdf_wrapper / freetype / harfbuzz, so the rest of
    # MuPDF's dependency set has to be added on the link line.
    extra_link_libs=("mupdf-third", "gumbo", "jbig2dec", "jpeg",
                     "openjp2", "z"),
)

KINDLE = Profile(name="kindle")

PROFILES = {"host": HOST, "kindle": KINDLE}

SOURCES = {
    # sha256 recorded 2026-08-09 (TOFU, see module docstring).
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


def extract(source: Source) -> None:
    if (source.src_dir / ".xbuild-extracted").exists():
        return
    log(f"extracting {source.name} {source.version}")
    shutil.rmtree(source.src_dir, ignore_errors=True)
    source.src_dir.mkdir(parents=True)
    with tarfile.open(source.tarball) as tf:
        members = []
        for m in tf.getmembers():
            parts = Path(m.name).parts
            if len(parts) < 2:
                continue
            m.name = str(Path(*parts[1:]))     # --strip-components 1
            members.append(m)
        tf.extractall(source.src_dir, members=members, filter="tar")
    (source.src_dir / ".xbuild-extracted").touch()


def check_system_deps(profile: Profile) -> None:
    missing = [p for p in profile.pkgconfig_libs
               if subprocess.run(["pkg-config", "--exists", p]).returncode != 0]
    if missing:
        die(f"missing pkg-config packages: {', '.join(missing)}\n"
            f"    brew install {' '.join(profile.brew_packages)}")


def cargo_env(profile: Profile) -> dict:
    env = dict(os.environ)
    flags = []
    for d in profile.link_search:
        flags += ["-L", f"native={d}"]
    for d in {Path(w[2:]) for lib in profile.pkgconfig_libs
              for w in pkgconfig(lib, "--libs").split() if w.startswith("-L")}:
        flags += ["-L", f"native={d}"]
    for lib in profile.extra_link_libs:
        flags += ["-l", lib]
    env["RUSTFLAGS"] = " ".join(flags + [env.get("RUSTFLAGS", "")]).strip()
    return env


def build_host(profile: Profile, args) -> None:
    if platform.system() != "Darwin":
        die("the 'host' profile is macOS-only; add a branch here for Linux.")
    check_system_deps(profile)
    fetch_release_assets()
    for name in profile.sources:
        source = SOURCES[name]
        fetch(source)
        extract(source)
        if source.build and not (source.src_dir / ".xbuild-built").exists():
            log(f"building {source.name}")
            source.build(source.src_dir, profile)
            (source.src_dir / ".xbuild-built").touch()
        elif source.build:
            profile.link_search.append(source.src_dir / "build" / "release")
    log("building mupdf_wrapper")
    build_mupdf_wrapper(profile)

    cmd = ["cargo", "run" if args.run else "build", "-p", profile.cargo_package]
    if profile.cargo_features:
        cmd += ["--features", ",".join(profile.cargo_features)]
    cmd += args.cargo
    log(" ".join(cmd))
    run(cmd, cwd=ROOT, env=cargo_env(profile))


def build_kindle(profile: Profile, args) -> None:
    die("the 'kindle' profile lands in phase 1 (see docs/plato-port.md in "
        "the ezkindle repo): zig cc -target arm-linux-musleabi, static .a "
        "output, cargo-zigbuild, armv7-unknown-linux-musleabi. Not "
        "implemented yet.")


BUILDERS = {"host": build_host, "kindle": build_kindle}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("profile", choices=sorted(PROFILES))
    p.add_argument("--run", action="store_true",
                   help="cargo run instead of cargo build")
    p.add_argument("--clean", action="store_true",
                   help="remove .xbuild/ (keeping the verified tarballs)")
    p.add_argument("cargo", nargs="*",
                   help="extra arguments passed through to cargo")
    args = p.parse_args()

    if args.clean:
        for child in WORK.glob("*"):
            if child != CACHE:
                shutil.rmtree(child, ignore_errors=True)

    profile = PROFILES[args.profile]
    BUILDERS[args.profile](profile, args)


if __name__ == "__main__":
    main()
