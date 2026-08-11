# foldersync

Mirror a folder from a computer on the LAN into a Plato library, from the
Applications menu.

Two binaries, no dependencies beyond `std` on either side:

- **`folder_hub`** runs on the computer that holds the documents. It serves
  one folder read-only over HTTP and answers UDP discovery probes.
- **`folder_fetcher`** runs on the reader, started from **Applications → Sync**.
  It speaks Plato's [fetcher protocol](../../doc/HOOKS.md), including asking for
  WiFi, so on a device where the radio is off by default the radio is on for
  exactly the length of the sync.

```
            Applications ▸ Sync
  Plato ──────────────────────────► folder_fetcher
        ◄── {"type":"setWifi"} ────
        ─── {"network":"up"} ─────►
                                      │  who is serving? (cache, config, probe)
                                      ▼
                                   folder_hub ──► GET /manifest, GET /file/…
        ◄── {"addDocument"} × n ───
```

**It is an application, not a `Hook`** — and that is a correction, not a
preference. Attached as a hook it ran on *navigation*: every glance at the
directory started a network operation, and the ordinary way out of the
directory — opening a document — killed the transfer and reported it to the
user as "abnormal process termination". Syncing is something you ask for.

## Why it looks like this

It was written for a jailbroken Kindle Paperwhite 3: Linux 3.0.35, armv7
soft-float, an OpenSSL that predates TLS 1.3, **an empty CA store**, and a
real-time clock that believes it is 2023. Anything involving public-CA TLS
fails there on three independent counts, and every dependency is another thing
to cross-compile for a soft-float target.

So the transport is plain HTTP on the LAN, the manifest is a tab-separated
table, and the whole protocol is a few hundred lines of `std`. The device side
is a 540 KB static binary.

The same reasoning produced three choices worth naming:

- **UDP broadcast discovery, not mDNS.** The device has no mDNS resolver — no
  avahi, no `nss-mdns` — so `computer.local` does not resolve, and multicast on
  a cheap SDIO WiFi part is not something to bet a feature on. Since both ends
  are ours, a broadcast probe answered by unicast is smaller *and* more
  reliable. The last working address is cached, so on a stable network the
  discovery step never runs at all.
- **The hub states the time, and the reader takes it.** The manifest header
  carries the hub's clock preformatted for `date -s`, so a device with no
  working clock and no way to reach an NTP server gets a correct one as a side
  effect of syncing. Set `set_time = false` to opt out.
- **Additive only.** A file deleted on the computer is never deleted on the
  reader. Deleting would also discard reading position, and a sync that can
  delete is a sync that can delete *everything* the first time the folder is
  mistyped.

## Running the hub

```sh
folder_hub --root ~/Documents/reader [--port 8571] [--disco-port 30303] [--token SECRET]
```

Only files whose extension Plato can open are listed or served (`epub`, `pdf`,
`cbz`, `djvu`, `fb2`, `xps`, `mobi`, `txt`, `html`), so the folder can also hold
notes and whatever else accumulates. Subdirectories are mirrored.

There is no write path. A request is served only if its canonicalized path is
still inside `--root`, which also covers symlinks pointing out of the folder.

`--token` is a shared secret checked on both the discovery probe and every HTTP
request. A bad token is answered with silence on UDP and `403` on TCP. It is a
LAN convenience, not a security boundary — the traffic is plaintext.

## Installing the fetcher

Put the binary somewhere under Plato's working directory and name it in
`Settings.toml`:

```toml
[sync]
program = "bin/folder_fetcher/folder_fetcher"
path = "papers"          # where documents land, relative to the library root
```

The Applications menu offers **Sync** only when `program` exists, so a reader
without the fetcher never sees an entry that cannot work.

Optional `folder_fetcher.conf`, read from the binary's own directory:

```
# hub        =          # host[:port] to try before broadcasting
# disco_port = 30303
# token =
# set_time  = true    # adopt the hub's clock when ours is off by over an hour
# wifi_wait = 60      # seconds to wait for the network after asking for WiFi
# timeout   = 20      # seconds per network operation
```

`.last-hub` is written alongside it and holds the last address that worked.
Delete it to force a fresh discovery.

**Set `hub` if discovery does not find the computer.** Prefer a *name* —
addresses move, names do not, and the name is resolved on every run. Every
address the name resolves to is tried, in order: a home router will happily
serve a name's stale leases alongside its live one, so taking only the first
answer is a coin flip. Known addresses — the
cached one, then `hub` — are tried before any broadcast goes out, so on a stable
network the sync starts instantly and on an access point that declines to
forward broadcast between clients it works at all. That case is real and was
what this ran into first: unicast to the hub worked perfectly while every probe
vanished.

## Protocol

Discovery, on UDP:

```
device -> 255.255.255.255:30303   PLATOSYNC1 DISCOVER <token>
hub    -> back to the sender      PLATOSYNC1 OFFER <http-port> <epoch>
```

The manifest, `GET /manifest`, tab-separated:

```
#PLATOSYNC1 <epoch> <YYYY-MM-DD HH:MM:SS>      (the time is UTC)
<size>	<mtime-epoch>	<relative/path.epub>
```

Files, `GET /file/<percent-encoded relative path>`. HTTP/1.0 with
`Connection: close` throughout: the body is everything until EOF, so there is
no chunked decoding and no keep-alive state on either side.

A file is fetched when its name is absent locally or its size differs. There is
no hashing — the reader is a 1 GHz Cortex-A9 and would spend longer digesting a
PDF than downloading it. Downloads land on `<name>.part` and are renamed only
after the length matches, so the routine `SIGTERM` mid-transfer cannot leave a
truncated file that looks complete.
