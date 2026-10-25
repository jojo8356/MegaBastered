# MegaBastered

A fast, lightweight MEGA.nz client written in Rust with an interactive CLI interface.

No Java, no Electron, no bloat. Just a single binary that handles downloads, uploads, streaming and account management through the MEGA API.

## Features

- **Multi-connection downloads** - Parallel chunk-based downloads with configurable workers (1-20)
- **Encrypted uploads** - AES-128-CTR encryption with proper MAC computation
- **Streaming** - Decrypt and pipe media files to mpv, vlc, or ffplay
- **Account management** - Login with email/password, 2FA support, session handling
- **Cloud file browser** - List files in tree view with handles for easy reference
- **Folder downloads** - Download a whole public folder (or sub-folder): tree is recreated, every file is decrypted with its own node key, already downloaded files are skipped
- **Resumable transfers** - Per-chunk index (`.megacli.parts`): an interrupted download resumes exactly where it stopped, and a partial file is never reported as complete
- **Quota aware** - HTTP 509 (MEGA transfer quota) stops the run cleanly and can wait/resume on its own (`--quota-wait`)
- **Scriptable CLI** - `megacli <link>` to download, `megacli --list <link> [-d dest]` to see what is already done
- **Instant re-runs** - a concurrent pre-scan of the destination lists what is already complete, those files are never touched again and their leftovers are cleaned
- **Quota monitoring** - Check storage usage and remaining space
- **Directory creation** - Create folders in your MEGA cloud
- **Progress bars** - Real-time download/upload progress with speed and ETA
- **Configurable** - Download path, parallel connections, speed limits

## Tech Stack

| Component | Implementation |
|-----------|---------------|
| Runtime | Tokio (async) |
| HTTP | reqwest |
| Crypto | AES-128-CBC/ECB/CTR, RSA, PBKDF2-HMAC-SHA512 |
| CLI | dialoguer + console + indicatif |
| Serialization | serde_json, base64 |

## Build

```bash
# Debug
cargo build

# Release (optimized, stripped, ~3.7MB)
cargo build --release
```

Binary output: `target/release/megacli`

## Usage

```
$ ./megacli

MegaCLI v0.1.3
Client MEGA en Rust - Download/Upload/Stream

> Action
  1. Telecharger (lien MEGA)
  2. Telecharger depuis le cloud
  3. Uploader un fichier
  4. Lister les fichiers cloud
  5. Streamer un fichier
  6. Espace disque / Quota
  7. Creer un dossier
  8. Gerer les comptes
  9. Parametres
  0. Quitter
```

`q` (or Escape) quits from the main menu, and `q` at any "press Enter"
prompt exits the app. Same idea in the sub-menus: `q` goes back.

### Command line (scriptable, no menu)

```bash
# Inspect a folder: every file with [ok] / [reprendre] / [manquant],
# plus the remaining volume and an estimate of quota sessions
megacli --list "https://mega.nz/folder/<id>#<key>" -d ~/Documents/Formations

# Download everything (resumes on the next run)
megacli "https://mega.nz/folder/<id>#<key>" -d ~/Documents/Formations -w 8

# Same, but wait 30 min and continue automatically when MEGA's quota hits
megacli "https://mega.nz/folder/<id>#<key>" -d ~/Documents/Formations -q 30
```

Exit codes: `0` success, `1` error, `2` MEGA quota reached (safe to relaunch later).
Running it again with the same arguments skips finished files and resumes the others.

### MEGA transfer quota (HTTP 509)

MEGA limits how much a **free** account / anonymous visitor can download: roughly
**5 GB per rolling 6-hour window, per IP address** ([MEGA Help](https://help.mega.io/plans-storage/space-storage/transfer-quota)).
A 30 GB folder therefore needs several sessions spread over a day or two.

What this client does about it:

- HTTP 509 is detected per chunk and the whole run stops immediately (no point
  hammering the remaining files) with the remaining volume and a resume hint;
- everything already downloaded is kept, `.megacli.parts` records the completed
  chunks so the next run only fetches what is missing;
- the next run starts with a verification pass: finished files (size matches) are
  skipped without a single API call, stale `.megacli.tmp`/`.megacli.parts` are
  removed, and only missing or partial files are queued;
- `--quota-wait <minutes>` (menu: *Parametres → 4*) pauses and retries
  automatically instead of stopping;
- a paid MEGA plan raises the limit (its quota is per account, not per IP).

The client does not rotate proxies/VPNs to farm free quota: that is against
MEGA's terms of service.

### Download a public link

Paste any `mega.nz` link (file or folder). Supports both old (`#!`, `#F!`) and new
(`/file/`, `/folder/`) URL formats.

Folder links (`https://mega.nz/folder/<id>#<key>`) are fully supported:

1. the folder tree is fetched once (`a:f` with the folder key),
2. every node key is unwrapped with the folder key and used to decrypt its name,
3. each file gets its own download URL and is downloaded with the usual parallel
   chunk engine, decrypted chunk by chunk with AES-128-CTR,
4. empty folders are recreated too, and files already present with the right size
   are skipped, so re-running the same link resumes an interrupted transfer.

MEGA download URLs expire after a few minutes; they are renewed automatically
(up to 20 times per file) if the transfer is still running.

### Upload a file

Requires a logged-in account. Files are encrypted client-side before upload with a random AES key.

### Resume / partial downloads

While a file is downloading, data is written to `<name>.megacli.tmp` next to the
final file, and every finished chunk offset is appended to
`<name>.megacli.parts`. If the process is interrupted (Ctrl+C, network drop, MEGA
quota), run the same command again: the sidecar tells exactly which chunks are
already on disk, the others are fetched, and the file is renamed only once it is
complete and the right size. A short recap is printed at the end of a folder
download (downloaded / already present / failed / quota reached).

### Stream media

Downloads and decrypts a file, then launches your preferred media player.

## Architecture

```
src/
├── main.rs       # Interactive menu, app state
├── api.rs        # MEGA API client (login, nodes, upload/download URLs)
├── crypto.rs     # AES-CBC/ECB/CTR, RSA, PBKDF2, MEGA key derivation
├── download.rs   # Multi-connection parallel download engine
└── upload.rs     # Encrypted upload with chunk MAC
```

### MEGA Crypto

- **Login v1** (legacy): AES-CBC key derivation (65536 rounds)
- **Login v2**: PBKDF2-HMAC-SHA512 (100K iterations)
- **File encryption**: AES-128-CTR with per-file random keys
- **Key wrapping**: AES-ECB for node keys, XOR for link keys
- **MAC**: Per-chunk AES-CBC MAC, XOR-folded into file MAC

### Download Engine

Files are split into adaptive chunks (128KB to 20MB) following MEGA's chunk size algorithm. Multiple tokio workers fetch chunks in parallel, decrypt with AES-CTR at the correct stream offset, and write to a pre-allocated temp file. On completion, the temp file is renamed to the final filename.

## Tests & CI

```bash
cargo test
```

Unit tests cover the MEGA chunk layout, per-chunk AES-CTR decryption against a
single stream, node-key unwrapping (including multi-segment imported keys), the
resume sidecar, quota error detection and folder path rebuilding.

`.github/workflows/ci.yml` runs `cargo check`, `cargo test`, `cargo clippy` and a
release build on every push.

## Requirements

- Rust 1.70+
- Linux / macOS / Windows

## License

MIT
