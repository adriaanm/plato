#!/usr/bin/env python3
"""Field-verify a deployed `platonic-recv`, from the Mac, with the admin key.

Run it after deploying the binary and before trusting a paired key with
anything.  Every step prints what it proves and, where there is one, the way it
could pass while the thing it checks is broken.

    python3 crates/platonic-recv/check.py [--host ADDR] [--keep]

It uses the ADMIN key (`~/.ssh/platokin_ed25519`), because several steps need a
shell to look at the result -- that is the point: the receiver is checked from
outside itself.  The restricted-key half is the one thing here that is NOT
automated; see the note at the end.

The frames below are hand-encoded rather than emitted by the Rust client.  That
is deliberate for the negative tests: a second, independent implementation of
the format means a drift between the two ends fails loudly here instead of
passing quietly on both sides of one shared bug.
"""

import argparse
import os
import struct
import subprocess
import sys
import time

GREETING = b"PLATONIC-RECV/1\n"
RECV = "/var/local/ezssh/platonic-recv"
DOCROOT = "/mnt/us/documents"
PUT, OPEN, IMPORT, LIST, SWEEP, QUIT = 1, 2, 3, 4, 5, 6
OK, INVALID, NOTFOUND, IO, UNSUPPORTED = 0, 1, 2, 3, 4
STATUS = {OK: "Ok", INVALID: "Invalid", NOTFOUND: "NotFound", IO: "Io",
          UNSUPPORTED: "Unsupported"}

PASS, FAIL = "PASS", "FAIL"
failures = []


def report(name, ok, proves, false_positive=None, detail=""):
    print(f"[{PASS if ok else FAIL}] {name}")
    print(f"       proves: {proves}")
    if false_positive:
        print(f"       false positive: {false_positive}")
    if detail:
        for line in str(detail).strip().splitlines():
            print(f"       | {line}")
    if not ok:
        failures.append(name)


def ssh_argv(host, command):
    key = os.environ.get("PLATONIC_KEY",
                         os.path.expanduser("~/.ssh/platokin_ed25519"))
    known = os.environ.get("PLATONIC_KNOWN_HOSTS",
                           os.path.expanduser("~/.ssh/platokin_known_hosts"))
    return ["ssh", "-p", "2222", "-i", key,
            "-o", "HostKeyAlias=platokin",
            "-o", f"UserKnownHostsFile={known}",
            "-o", "StrictHostKeyChecking=yes",
            "-o", "BatchMode=yes", "-o", "ConnectTimeout=5",
            f"root@{host}", command]


def sh(host, command, stdin=b"", timeout=60):
    return subprocess.run(ssh_argv(host, command), input=stdin,
                          capture_output=True, timeout=timeout)


def s(value):
    raw = value.encode()
    return struct.pack(">H", len(raw)) + raw


def put_frame(folder, name, mtime, payload):
    return (bytes([PUT]) + s(folder) + s(name)
            + struct.pack(">qI", mtime, len(payload)) + payload)


def decode(stream):
    """Yield (status, body) for as many responses as the stream holds."""
    assert stream.startswith(GREETING), f"no greeting: {stream[:32]!r}"
    return stream[len(GREETING):]


def status_of(rest):
    return rest[0], rest[1:]


def talk(host, frames, timeout=120):
    out = sh(host, RECV, stdin=b"".join(frames), timeout=timeout)
    return out.stdout, out.stderr.decode(errors="replace"), out.returncode


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default=os.environ.get("PLATONIC_HOST", "platokin"))
    p.add_argument("--keep", action="store_true",
                   help="leave the test documents on the reader")
    args = p.parse_args()
    host = args.host
    now = int(time.time())

    # 1 ------------------------------------------------------------------
    out = sh(host, f"ls -l {RECV} 2>&1; md5sum {RECV} 2>/dev/null")
    listing = out.stdout.decode(errors="replace").strip()
    report("binary is deployed", "No such file" not in listing and listing != "",
           "the file exists on p3, with its mode and its md5",
           "an OLD build with the same name -- compare the md5 with "
           "`md5 target/armv7-unknown-linux-musleabi/release/platonic-recv`",
           listing)
    report("mode is 755", " -rwxr-xr-x " in f" {listing} ",
           "p3 is group-writable, so the mode is worth asserting rather than "
           "assuming", "none", listing)

    # 2 ------------------------------------------------------------------
    stream, err, code = talk(host, [])
    report("it greets", stream.startswith(GREETING),
           "the binary runs on this device's ABI and libc, and canonicalised "
           f"{DOCROOT} -- the containment boundary exists",
           "none: no greeting is emitted before the root resolves",
           f"stdout={stream[:40]!r} rc={code} {err}")
    if not stream.startswith(GREETING):
        print("\nnothing else can be checked; stopping.")
        return 1

    # 3 ------------------------------------------------------------------
    tty = subprocess.run(["ssh", "-tt"] + ssh_argv(host, RECV)[1:],
                         input=b"", capture_output=True, timeout=30)
    blob = (tty.stdout + tty.stderr).decode(errors="replace")
    report("it refuses a tty", "stdin is a terminal" in blob or "PTY" in blob,
           "a hand-run receiver says so instead of hanging on a framed "
           "protocol nobody is speaking",
           "with the ADMIN key dropbear grants the pty and the receiver "
           "refuses; with a paired key dropbear refuses the pty first (E22). "
           "Either message is a pass, and they prove different things.",
           blob.strip()[:200])

    # 4 ------------------------------------------------------------------
    body = f"# platonic-recv check {now}\n\nIf you can read this, PUT works.\n"
    body = body.encode()
    stream, err, _ = talk(host, [
        put_frame("inbox", f"recv-check-{now}.md", now, body),
        put_frame("papers", f"recv-check-keep-{now}.md", 1, b"keep me\n"),
        bytes([QUIT]),
    ])
    rest = decode(stream)
    st, rest = status_of(rest)
    written = struct.unpack(">Q", rest[:8])[0] if st == OK else -1
    rest = rest[8:] if st == OK else rest
    report("PUT writes the exact byte count", written == len(body),
           "the document arrived whole -- the count comes from the writer, "
           "not from a second `wc -c`",
           "a previous run's file of the same size; the name carries a "
           "timestamp to stop that",
           f"status={STATUS.get(st)} written={written} sent={len(body)}")

    out = sh(host, f"stat -c '%Y %s %n' {DOCROOT}/inbox/recv-check-{now}.md")
    stat_line = out.stdout.decode().strip()
    report("PUT sets the Mac's mtime", stat_line.startswith(str(now)),
           "expiry is decided against the Mac's clock, so the mtime is as "
           "much a part of the document as the bytes",
           "the device clock happening to agree -- it does after clock-sync.sh, "
           "which is why the value is compared, not the year",
           stat_line)

    # 5 ------------------------------------------------------------------
    escapes = [("..", "escaped.md"), ("../..", "escaped.md"),
               ("inbox", "../../escaped.md"), (".ssh", "authorized_keys"),
               ("inbox", "-rf"), ("inbox", "a\nb.md")]
    frames, expected = [], []
    for folder, name in escapes:
        frames.append(put_frame(folder, name, now, b"PWNED"))
        expected.append((folder, name))
    frames.append(bytes([QUIT]))
    stream, err, _ = talk(host, frames)
    rest = decode(stream)
    refused, detail = 0, []
    for folder, name in expected:
        st, rest = status_of(rest)
        n = struct.unpack(">H", rest[:2])[0]
        msg, rest = rest[2:2 + n].decode(errors="replace"), rest[2 + n:]
        detail.append(f"{folder}/{name} -> {STATUS.get(st)}: {msg}")
        if st == INVALID:
            refused += 1
    report("hostile names are refused ON THE DEVICE", refused == len(expected),
           "validation is enforced by the root program, not by the Mac that "
           "chose to send well-formed names",
           "a receiver that refused for the wrong reason -- read the messages",
           "\n".join(detail))

    out = sh(host, "ls /mnt/us/escaped.md /mnt/escaped.md /escaped.md "
                   "2>&1 | grep -c 'No such'")
    report("nothing landed outside the library", out.stdout.decode().strip() == "3",
           "the refusals were refusals, not writes with a bad exit code",
           "a path this check did not think to look at; the containment "
           "assertion is what actually holds, and it is unit-tested",
           out.stdout.decode().strip())

    # 6 ------------------------------------------------------------------
    stream, err, _ = talk(host, [
        put_frame("inbox", f"recv-check-old-{now}.md", 1_000_000, b"ancient\n"),
        struct.pack(">Bq", SWEEP, 2_000_000),
        bytes([QUIT]),
    ])
    rest = decode(stream)
    st, rest = status_of(rest)
    rest = rest[8:]
    st, rest = status_of(rest)
    swept = []
    if st == OK:
        count = struct.unpack(">I", rest[:4])[0]
        rest = rest[4:]
        for _ in range(count):
            n = struct.unpack(">H", rest[:2])[0]
            swept.append(rest[2:2 + n].decode(errors="replace"))
            rest = rest[2 + n:]
    report("SWEEP expires inbox by the Mac's clock",
           f"recv-check-old-{now}.md" in swept,
           "the ephemerality rule works with no shell involved",
           "an empty inbox would also produce no complaint -- the file was "
           "created by this same session, one frame earlier",
           f"swept: {swept}")

    out = sh(host, f"ls {DOCROOT}/papers/recv-check-keep-{now}.md")
    report("SWEEP leaves named folders alone",
           "No such" not in out.stdout.decode() + out.stderr.decode(),
           "a named folder means 'keep this'; the sweep folder is hard-wired "
           "to inbox and is not a parameter",
           "the sweep cutoff not covering it -- it was stamped mtime 1, which "
           "is older than everything",
           out.stdout.decode().strip() or out.stderr.decode().strip())

    # 7 ------------------------------------------------------------------
    stream, err, _ = talk(host, [bytes([LIST]), bytes([QUIT])])
    rest = decode(stream)
    st, rest = status_of(rest)
    count = struct.unpack(">I", rest[:4])[0] if st == OK else -1
    report("LIST answers", count > 0,
           "--list needs no shell either",
           "an empty library would read as a failure here; this run has just "
           "written two files",
           f"status={STATUS.get(st)} entries={count}")

    # 8 ------------------------------------------------------------------
    stream, err, _ = talk(host, [
        bytes([OPEN]) + s("inbox") + s(f"recv-check-{now}.md"),
        bytes([QUIT]),
    ])
    rest = decode(stream)
    st, rest = status_of(rest)
    report("OPEN reaches Plato's FIFO", st == OK,
           "LOOK AT THE READER: it should now be showing "
           f"'platonic-recv check {now}'",
           "NotFound here means Plato is not running its listener, which is a "
           "true answer about the reader, not a receiver fault",
           f"status={STATUS.get(st)}")

    if not args.keep:
        sh(host, f"rm -f {DOCROOT}/inbox/recv-check-*.md "
                 f"{DOCROOT}/papers/recv-check-*.md")

    print()
    if failures:
        print(f"FAILED: {', '.join(failures)}")
    else:
        print("all checks passed")
    print("""
NOT checked here, and it is the one that matters most -- do it by hand,
following E22's pattern (a throwaway dropbear on port 2223, so the live
authorized_keys on p3 is never at risk):

  1. a key whose entry is
     command="/var/local/ezssh/platonic-recv",no-port-forwarding,
     no-agent-forwarding,no-X11-forwarding,no-pty <pubkey>
  2. `ssh -p 2223 … 'id'` must print the GREETING, not an id
  3. `ssh -p 2223 … 'cat > /tmp/should-not-exist'` must leave no such file
  4. `PLATONIC_KEY=<that key> platonic doc.md` must push and open

Only step 4 proves the paired path end to end; steps 2 and 3 prove that the
forced command is still the boundary this design rests on.""")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
