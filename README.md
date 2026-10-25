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
- **Resumable transfers** - Interrupted downloads restart where they stopped (`.megacli.tmp`)
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

MegaCLI v0.1.0
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
final file. If the process is interrupted (Ctrl+C, network drop, MEGA quota), run
the same command again: completed chunks are detected from the temp file size and
only the missing ones are fetched. A short recap is printed at the end of a folder
download (downloaded / already present / failed).

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
single stream, node-key unwrapping (including multi-segment imported keys),
resume planning and folder path rebuilding.

`.github/workflows/ci.yml` runs `cargo check`, `cargo test`, `cargo clippy` and a
release build on every push.

## Requirements

- Rust 1.70+
- Linux / macOS / Windows

## License

MIT
