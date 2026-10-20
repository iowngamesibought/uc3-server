# Instructions for agents helping a user set up uc3-server

You are helping someone run this server for their own copy of the game. Work through the steps below in order, and check each one before moving on. Read `README.md` first for what the server does.

## Ground rules

- **Use only the user's own copy of the game.** Never download game files, decrypted executables, keys or license files from the internet, and never suggest where to get them. If the user doesn't have the files, stop and tell them what's missing.
- **Never put game-derived files in this repository**: executables, archives, generated output, decompiler projects or exports, server logs, stored uploads. Keep them in a folder outside it, or in the git-ignored `game/`, `logs/` and `storage/` folders.
- **The host names you find in step 2 go into the user's DNS setup only.** Don't write them into this repository (code, docs, config examples, commits or issues), and don't copy anything else out of the executable: no keys, code or text. The server reads what it needs from the user's files at runtime.
- Run commands only with the user's agreement when they change their system (firewall rules, DNS, router settings).

## 1. Supply the required files

The server needs two files from the user's own installed copy, plus one optional file. This guide deliberately doesn't name them: identify them from the descriptions below. They go into `uc3-server.toml` (copy it from `uc3-server.example.toml`) under `executable`, `patch_archive` and `text_archive`; relative paths resolve from the config file's folder.

### Where to look

Ask the user where their copy of the game is, which platform it's for, and whether the game's latest update is installed; they can give you the folders and that context. Two places matter:

- The **update's data folder**: the latest update installs its own main executable and its patch archives there, directly in the folder (no `build/` tree).
- The **base game's data folder** (on the disc or in the installation): it holds an older main executable and a `build/main/` tree of archives.

The server supports only the executable of the game's **latest update**: the game is no longer developed, so that update is final. If it isn't installed, the user has to install it on their console first.

Copying the files off the console (or off the disc) is the user's job, like decrypting the executable below; ask them for the files once they have them on this machine.

Copy the files into a `game/` folder next to the user's `uc3-server.toml` (git-ignored in this project), keeping their original names, and set the keys to `game/<file name>`. Absolute paths elsewhere work too. Never place them anywhere that gets committed.

### What each file is

| Config key | How to recognise it |
|---|---|
| `executable` | The **update's main boot executable**: the one file directly in the update's data folder with a `.BIN` extension, about 18.8 MB. As installed it is **encrypted** (it does not start with `\x7fELF`). The server needs it **decrypted** to an ELF: starts with `\x7fELF`, then bytes `02 02` (64-bit, big-endian). The user must provide that decrypted form from their own copy; it is their responsibility. Don't explain how to decrypt it, and never obtain a decrypted copy elsewhere. The base game's executable (also `.BIN`, slightly smaller) is an older build and won't work. |
| `patch_archive` | One of the update's **patch archives**, directly in the update's data folder, a few hundred MB, in the game's own archive format. Its entries are full paths starting with `build/main/`. The update has two of them; the right one holds the online config module (entries ending in `dc1/net.bin` and the update's own `dc1/net-patch<N>.bin`) and the per-language online texts (`build/main/text1/<n>.networking`). It is the larger of the two, with the higher number in its name. |
| `text_archive` (optional) | The base game's **text archive** in its `build/main/` folder, about 5 MB. Its entries are per-language text tables named `<number>.<category>` (e.g. `2.common`), including `<number>.networking` for each language. Usually not needed: the latest update's patch archive already has the online texts of every language. Adding it does no harm. |

The base game's folders hold other archives too, including very large ones (hundreds of MB to GB) with map and character data, and another patch archive that came with the game's edition. None of those is right.

### Check each candidate with the server itself

- `uc3-server list-archive <file>` lists an archive's entries and ends with a verdict: `fits patch_archive`, `fits text_archive` or `fits neither`. It only reads the file.
- For the executable, start the server (next step). The startup log tells you whether it's right.

**Check:**
1. Set `public_host` to the server machine's LAN address as the console sees it (not `0.0.0.0`, not a placeholder).
2. Run `cargo run --release -- run --config uc3-server.toml` from this repository. There is no prebuilt binary: if `cargo` is missing, install the Rust toolchain first (rustup, https://rustup.rs).
3. The log must show:
   - `cipher keys found build=1.19` and `frame key found build=1.19`. Errors and what they mean:
     - **"not a big-endian ELF64"**: the executable is still encrypted, or it's the wrong file.
     - **"N known key locations match"** or **"the frame key location of no supported build"**: it's not the main executable of the game's latest update. Usually it is the base game's older one, or one from a copy without the latest update. If the user is sure it comes from the latest update, stop and report it: the server only knows the key locations of the update build it was tested with, and supporting another build needs a code change.
   - `online config module found entry=... servers=26`. A **"not the patch data archive"** error means `patch_archive` points at the wrong archive.
   - `EULA pages served by this server`, `level download list served empty` and `matchmaking playlists ...`.
   - `online config ready (in memory)`, then `networking texts ready indexes=...` listing language indexes. A **"no <n>.networking text found"** warning means neither archive has the online texts.
   - `live-data HTTP listening`, `framed services listening` (three ports), `location service listening` and `matchmaking listening`. A bind error usually means a port is in use (e.g. port 80 by IIS) or another server instance is running.

## 2. Find the host names in the user's executable

This guide deliberately doesn't name the hosts, and no public packet captures of this game's online mode are known. Work the names out from the **user's own decrypted executable** (the file set as `executable`). Write only the host names down, and don't commit them or anything else from the executable.

### What the client does

- It downloads its online config, its per-language texts and the player's profile over **plain HTTP** (port 80, no TLS) from a **cloud object-storage bucket addressed by host name**: `<bucket name>.<storage provider's domain>`. This is the **config host**. Requests look like `/<name>.bin.psarc.crypt`, `/build/main/text1/<n>.networking.crypt` and `/profiles/<online id>/profile.<n>`.
- It **uploads** player profiles and match records to the same provider in **path style**: the host is the provider's bare domain, and the path is `/<bucket name>/<key>`. This is the **upload host**. Without it, sign-in and matches still work, but profiles and match records are never saved.
- It can also build a second base address from the same bucket name under the **game developer's own domain** (the **content host**). It wasn't seen being contacted in testing; mapping it is optional and harmless.
- Every other address the client uses (the service ports, the terms pages) comes from the config this server generates, so no other names are needed.

### How to find them

Use `strings -n 6 <executable>` (Linux, macOS), or this Python equivalent (any platform; it only reads the file):
```
python -I -c "import re,sys;[print(m.group().decode()) for m in re.finditer(rb'[\x20-\x7e]{6,}',open(sys.argv[1],'rb').read())]" <executable>
```
Pipe the output into `grep`/`findstr` rather than saving it, or save it outside the repository.

1. **The two domains:** the strings starting with `http://%s.` are URL formats: there are exactly two, `http://%s.<domain>`. To tell them apart:
   - The **storage provider's domain** also appears elsewhere on its own: as a bare string (exactly the domain) and as `%s.<domain>` without the scheme. The bare string is the upload host.
   - The **developer's domain** appears in many e-mail addresses and internal host names, never on its own.
2. **The bucket name:** look for short dotted names ending in `.dev` or `.prod` (for example with the pattern `^[a-z0-9]+(\.[a-z0-9]+)*\.(dev|prod)$`). There are a few pairs with the same game-specific prefix. The live-data bucket is the **shortest one ending in `.prod`**; the longer ones, with an extra word in the middle, hold other content (videos, beta builds). Retail accounts always get the production bucket: the client only uses the `.dev` one for development sign-ins.
3. **Compose the names:**
   - config host = `<bucket>.<storage provider's domain>`
   - upload host = `<storage provider's domain>` (the bare domain)
   - content host (optional) = `<bucket>.<developer's domain>`
4. **Sanity checks:**
   - The executable also holds at least one complete URL of the form `http://<bucket>.<storage provider's domain>/...`. It must start with the config host you composed.
   - Neither name belongs to the console platform's own vendor.
   - Don't override anything else: not the provider's message-queue host, not social-network hosts, not the platform's domains.

**Optional confirmation with a decompiler:** load the executable (a 64-bit big-endian PowerPC ELF) in a decompiler such as Ghidra and look at the references to the two `http://%s.` strings. The online start-up function uses both. It also references the bucket names and picks one by comparing the sign-in ticket's issuer id with 8: the development bucket below 8, otherwise production. It then builds both base addresses from that bucket and requests the config archive from the storage-provider one. Keep the decompiler project and any exports outside this repository.

## 3. Set up the DNS override

Map the names to `public_host` on a DNS server the console uses. For dnsmasq:
```
address=/<config host>/<public_host>
address=/<upload host>/<public_host>
address=/<content host>/<public_host>
```

dnsmasq's `address=` also matches every subdomain of the name. So the upload-host line on its own already sends every bucket of the storage provider to the server, the config host included. That is fine on a DNS server only the console uses, and it is why the next point matters.

The upload host is the storage provider's main domain, which many other devices and services rely on. **Override it for the console only.** The simplest way: run a small DNS forwarder just for the console (e.g. dnsmasq on the server machine, with the lines above and the normal upstream resolver for everything else), and set the console's DNS server manually to it in its network settings. Changing the router's DNS for the whole network would cut every other device off from that provider. The config host can be overridden network-wide if that's easier.

**Proxies:** if a proxy or TUN (fake-IP) sits between the console and the internet, add the names to its fake-IP filter and route them DIRECT. Otherwise the proxy answers with its own addresses and the console never reaches the server.

**Check:**
1. `nslookup <config host> <console's DNS server>` must return `public_host`; the same for the upload host. Query the console's DNS server explicitly: the machine you run the check on may use a different resolver.
2. Allow inbound TCP 80, 6300–6303 and 5203 on the server machine.
3. Start the game's online mode. The server log (`RUST_LOG=debug` for more detail) should show, in order:
   - `http method=GET host=<config host> path=/<name>.bin.psarc.crypt status=200`: twice a few milliseconds apart when the console downloads a changed config, once when its cached copy is current;
   - `login ticket ... online_id=<the user's online id>` and `matchmaking client connected`;
   - `GET .../build/main/text1/<n>.networking.crypt 200`;
   - `GET /eula/page-<n>.png 200` when the console shows the terms pages;
   - `GET .../profiles/<id>/profile.86` (404 the first time is normal), `ban check: not banned`, `heartbeat`;
   - `stored upload key=profiles/<id>/profile.86` with `http method=PUT host=<upload host>`: this confirms the upload host.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| No request reaches the server | DNS still resolves the config host elsewhere (check against the console's DNS server), a proxy intercepts it, or the firewall blocks port 80. If the first config download fails, the console retries against an old built-in address that DNS can't redirect, so the online start then fails after a long wait. |
| Config downloaded, but no `login ticket` line | `public_host` isn't reachable from the console, or TCP 6300 is blocked |
| "Failed to initialize session manager" right after sign-in | TCP 6303 is blocked, or the matchmaking port failed to bind (see the startup log) |
| The terms pages fail to load | They come straight from `public_host` on the HTTP port, without DNS: check that the console can reach it |
| "Error syncing text" | The text for the console's language index wasn't served: add `text_archive` and check the `networking texts ready` log line |
| No `PUT` or `stored upload` lines; player progress isn't kept between sessions | The upload host isn't overridden for the console |
| Two consoles that start Find Game at the same moment each host their own lobby | Both searched before either lobby existed. Start the second console a few seconds later |
| Only one config `GET` per start, and config changes have no effect | The console's cached copy is considered current. Any change to the served config changes its date and forces a download at the next sign-in; check that the server was restarted after the config change |
