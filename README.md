# uc3-server

A community server for the online mode of **UC3** on certain game console.

This project is not affiliated with or endorsed by the game's developer or publisher. It ships no game files: the server reads what it needs from **your own installed copy** at startup and keeps everything it derives from it in memory only.

Status: tested with two consoles on one LAN. Sign-in, Find Game (competitive and co-op), custom games with invites and full matches, including the uploads after a match, all work. Leaderboards and invite lists answer with empty lists for now.

## What it serves

| Port | Service |
|---|---|
| TCP 80 | Live-data HTTP: the online config (generated from your patch data, with every server address pointed here), the per-language online texts, the terms pages, and player profile and match uploads |
| TCP 6300, 6302, 5203 | The client's framed services: login ticket, ban check and reports, heartbeat, invites, leaderboards, game list, text filter |
| TCP 6301 | Location (for geographic matchmaking) |
| TCP 6303 | Matchmaking rooms (the game's own lobby protocol) |

Platform services (sign-in and the peer connections between consoles) stay with the platform and are not provided here. Matches are hosted by one of the consoles, as before.

## Required: a DNS override

The console only reaches this server if **a few specific host names** it contacts resolve to this machine: the host it downloads its online config from, and the host it uploads player profiles and match records to. Every other address the console uses comes from the served config.

This README deliberately doesn't name them. No public packet captures of this game's online mode are known, so the names have to come from **your own copy of the game**. To find them, **ask an AI agent to follow [AGENTS.md](AGENTS.md)**: it reads the names from your own executable, then walks you through the override (a DNS server for the console, proxy exemptions) and checks that it works. Once you have the names, the setup itself is a few lines (see [Network setup](#network-setup)).

## Required: files from your copy of the game

The server ships no game data. It needs these files from **your own installed copy**, set in the config:

| Config key | What it is |
|---|---|
| `executable` | The main executable of the game's latest update, decrypted from your own installation. The server reads its keys from it at startup. |
| `patch_archive` | The latest update's patch data archive: the one holding the online config module and the per-language online texts. |
| `text_archive` (optional) | The base game's text archive, for languages the patch archive lacks. |

This README deliberately doesn't name the files. To locate them, **ask an AI agent to follow [AGENTS.md](AGENTS.md)**: it describes how to recognise each one in your install, and how to check its choice with the server (`uc3-server list-archive <file>` and the startup log). Never download these files from the internet; use only your own copy.

**Where to put them:** copy the files into a `game/` folder next to your `uc3-server.toml`, and point the keys at them:

```
uc3-server.toml
game/
  <main executable, decrypted>
  <patch data archive>
  <text archive>
```
```toml
executable    = "game/<main executable file name>"
patch_archive = "game/<patch data archive file name>"
text_archive  = "game/<text archive file name>"
```

Relative paths are resolved from the config file's folder, so the files can also live anywhere else if you give absolute paths. Keep the original file names; they're only needed in your config. `game/`, your `uc3-server.toml`, `logs/` and `storage/` are listed in this project's `.gitignore`, so they won't be committed by accident.

## Run

Building needs a Rust toolchain (https://rustup.rs); there is no prebuilt binary.

```
cp uc3-server.example.toml uc3-server.toml    # set public_host and the files above
cargo run --release -- run --config uc3-server.toml
```

`RUST_LOG=debug` shows every service connection and matchmaking record. Other commands:
- `list-archive <file>`: shows an archive's entries and whether it fits `patch_archive` or `text_archive`.
- `gen-config --out <dir>`: writes the generated config to a folder for inspection. That output is derived from your game files, so keep it local.

## Network setup

- **DNS:** map the names from [AGENTS.md](AGENTS.md) to this machine. With dnsmasq:
  ```
  address=/<config host>/<public_host>
  address=/<upload host>/<public_host>
  ```
  The upload host is a domain shared by many other services, so override it **for the console only**: give the console its own DNS server (for example a small dnsmasq on this machine) rather than changing the DNS of your whole network. If a proxy or TUN sits in front of the console, exempt the names from fake-IP and route them directly.
- **public_host:** this machine's LAN address as the console sees it. The server refuses to start with a placeholder or `0.0.0.0`; the bind addresses can stay `0.0.0.0`.
- **Firewall:** allow inbound TCP 80, 6300–6303 and 5203.
- **Cache:** the console caches the config keyed on `Last-Modified`. The server derives that date from the content, so restarts don't force a re-download, but any config change does.

## Tests

`cargo test` covers the ciphers (standard Blowfish vector, frame cipher reference vectors, round trips), the config module changes (server addresses, terms pages, level list), archive round trips, the framed service handshake over a socket, reply sizes against the client's buffers, the matchmaking room lifecycle (create, search, join, leave, host change, reconnects, stalled clients), HTTP caching headers, upload key rules and stored-file handling, damaged input files, and config validation.

## Security

The server is meant for a LAN or a group that trusts each other. Online ids come from the client and are not verified, and uploads are not authenticated: the server only accepts the key shapes the client uses (a match record only for an id it issued itself), limits their size and number, and never lets a connection from another address end a player's matchmaking session. Don't expose the ports to the internet unless you accept that players can see and change each other's uploaded profiles.

## Open questions

- Leaderboards and invites are received but not stored, so their lists stay empty.
- The text filter's reply format is not fully known; the server answers every request the same way.
- Room search ignores region and skill: any room of the same game type matches, which suits a small player base.
- Joining a match in progress has not been tested (it needs a third console).
