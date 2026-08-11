# foldersync

Mirror a folder from a computer on the LAN into a Plato library, as a
[fetcher hook](../../doc/HOOKS.md).

Two binaries, no dependencies beyond `std` on either side:

- **`folder_hub`** runs on the computer that holds the documents. It serves
  one folder read-only over HTTP and answers UDP discovery probes.
- **`folder_fetcher`** runs on the reader. Plato starts it when the user
  enters the hooked directory and `SIGTERM`s it when they leave, so the sync
  lasts exactly as long as the user is looking at the folder — and on a device
  where the radio is off by default, that is also exactly how long it is on.

```
                tap "Papers"
  Plato ──────────────────────────► folder_fetcher
        ◄── {"type":"setWifi"} ────
        ─── {"network":"up"} ─────►
                                      │  UDP broadcast: where are you?
                                      ▼
                                   folder_hub ──► GET /manifest, GET /file/…
        ◄── {"addDocument"} × n ───
```

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

Put the binary in the library, and point a hook at it:

```toml
[[libraries.hooks]]
path = "Papers"
program = "bin/folder_fetcher/folder_fetcher"
sort-method = "added"
```

Optional `folder_fetcher.conf`, read from the binary's own directory:

```
# disco_port = 30303
# token =
# set_time  = true    # adopt the hub's clock when ours is off by over an hour
# wifi_wait = 60      # seconds to wait for the network after asking for WiFi
# timeout   = 20      # seconds per network operation
```

`.last-hub` is written alongside it and holds the last address that worked.
Delete it to force a fresh discovery.

## Protocol

Discovery, on UDP:

```
device -> 255.255.255.255:30303   PLATOSYNC1 DISCOVER <token>
hub    -> back to the sender      PLATOSYNC1 OFFER <http-port> <epoch>
```

The manifest, `GET /manifest`, tab-separated:

```
#PLATOSYNC1 <epoch> <YYYY-MM-DD HH:MM:SS>
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
