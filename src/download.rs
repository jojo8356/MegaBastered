use anyhow::{anyhow, Result};
use ctr::cipher::StreamCipher;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};

use crate::api;
use crate::crypto::*;

/// Size of the big MEGA chunks, in MiB (used by the chunk size algorithm).
const CHUNK_SIZE_MULTI: u64 = 20;
const MAX_WORKERS: usize = 20;
const MAX_RETRIES: u32 = 8;
/// MEGA download URLs are valid for a few minutes only; renew them a bounded
/// number of times per file.
const MAX_URL_REFRESH: u64 = 20;
/// Suffix of the in-progress file.
const TEMP_SUFFIX: &str = ".megacli.tmp";

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Download a file (or a whole folder) from a public MEGA link.
pub async fn download_from_link(
    client: &Client,
    link: &str,
    dest_dir: &str,
    workers: usize,
    speed_limit: u64,
) -> Result<()> {
    if api::is_folder_link(link) {
        return download_folder(client, link, dest_dir, workers, speed_limit).await;
    }

    let link = api::normalize_mega_link(link);
    let (filename, file_size, file_key) = api::get_file_metadata(client, &link, None).await?;

    println!("  Fichier: {}", filename);
    println!("  Taille:  {}", api::format_bytes(file_size));

    let dest = Path::new(dest_dir).join(sanitize_file_name(&filename));
    let source = UrlSource::Link(link);

    if download_file(
        client,
        &source,
        file_size,
        &file_key,
        &dest,
        workers,
        speed_limit,
    )
    .await?
    {
        println!("\n  Sauvegarde: {}", dest.display());
    } else {
        println!("\n  Deja present: {}", dest.display());
    }

    Ok(())
}

/// Download every file of a public MEGA folder, recreating the directory tree.
pub async fn download_folder(
    client: &Client,
    link: &str,
    dest_dir: &str,
    workers: usize,
    speed_limit: u64,
) -> Result<()> {
    let (folder_id, folder_key) = api::extract_folder_id_and_key(link)?;

    println!("  Chargement du contenu du dossier...");
    let nodes = api::get_folder_nodes(client, &folder_id, &folder_key).await?;

    if nodes.is_empty() {
        return Err(anyhow!(
            "dossier vide ou inaccessible - verifie que le lien est toujours valide"
        ));
    }

    let paths = resolve_paths(&nodes, &folder_id);
    let base = Path::new(dest_dir);
    tokio::fs::create_dir_all(base).await?;

    // Recreate every folder, including the empty ones.
    let mut directories = 0usize;
    for node in nodes.iter().filter(|node| node.is_folder()) {
        if let Some(relative) = paths.get(&node.handle) {
            if tokio::fs::create_dir_all(base.join(relative)).await.is_ok() {
                directories += 1;
            }
        }
    }

    // Files, with their destination path.
    let files: Vec<(&api::FileNode, PathBuf)> = nodes
        .iter()
        .filter(|node| node.is_file())
        .map(|node| {
            let relative = match paths.get(&node.handle) {
                Some(path) => path.clone(),
                None => PathBuf::from(sanitize_file_name(&node.name)),
            };
            (node, relative)
        })
        .collect();

    let total_size: u64 = files.iter().map(|(node, _)| node.size).sum();
    println!(
        "  {} fichier(s), {} dossier(s), {} au total",
        files.len(),
        directories,
        api::format_bytes(total_size)
    );
    println!("  Destination: {}", base.display());

    if files.is_empty() {
        println!("\n  Aucun fichier a telecharger (dossiers vides recrees).");
        return Ok(());
    }

    let started = Instant::now();
    let mut downloaded = 0usize;
    let mut skipped = 0usize;
    let mut skipped_bytes = 0u64;
    let mut done_bytes = 0u64;
    let mut failures: Vec<(String, String)> = Vec::new();

    for (index, (node, relative)) in files.iter().enumerate() {
        let dest = base.join(relative);
        println!(
            "\n  [{}/{}] {} ({})",
            index + 1,
            files.len(),
            relative.display(),
            api::format_bytes(node.size)
        );

        if node.key.is_empty() {
            println!("  Echec: cle de dechiffrement indisponible");
            failures.push((relative.display().to_string(), "cle indisponible".to_string()));
            continue;
        }

        let source = UrlSource::FolderNode {
            folder_id: folder_id.clone(),
            handle: node.handle.clone(),
        };

        match download_file(
            client,
            &source,
            node.size,
            &node.key,
            &dest,
            workers,
            speed_limit,
        )
        .await
        {
            Ok(true) => {
                downloaded += 1;
                done_bytes += node.size;
            }
            Ok(false) => {
                skipped += 1;
                skipped_bytes += node.size;
                println!("  Deja present, ignore");
            }
            Err(e) => {
                println!("  Echec: {}", e);
                failures.push((relative.display().to_string(), e.to_string()));
            }
        }
    }

    // --- Recap --------------------------------------------------------------
    let elapsed = started.elapsed();

    println!("\n  ------------------------------------------------");
    println!(
        "  Termine en {} - {} telecharge(s), {} deja present(s), {} en echec",
        format_duration(elapsed),
        downloaded,
        skipped,
        failures.len()
    );
    if done_bytes > 0 {
        println!(
            "  {} transferes ({})",
            api::format_bytes(done_bytes),
            format_speed(done_bytes as f64 / elapsed.as_secs_f64().max(0.001))
        );
    }
    if skipped_bytes > 0 {
        println!("  {} deja sur le disque", api::format_bytes(skipped_bytes));
    }

    if !failures.is_empty() {
        println!("\n  Fichiers en echec:");
        for (path, error) in failures.iter().take(20) {
            println!("    - {}: {}", path, error);
        }
        if failures.len() > 20 {
            println!("    ... et {} autre(s)", failures.len() - 20);
        }
        println!("\n  Relance le meme lien pour reprendre les fichiers manquants.");
        return Err(anyhow!("{} fichier(s) en echec", failures.len()));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Download engine
// ---------------------------------------------------------------------------

/// Where the (short lived) download URL of a file comes from.
#[derive(Clone)]
enum UrlSource {
    /// A public file link (`#!` / `#N!` style).
    Link(String),
    /// A node of a public folder.
    FolderNode { folder_id: String, handle: String },
}

impl UrlSource {
    async fn fetch(&self, client: &Client) -> Result<String> {
        match self {
            UrlSource::Link(link) => api::get_file_download_url(client, link, None).await,
            UrlSource::FolderNode { folder_id, handle } => {
                api::get_node_download_url(client, folder_id, handle).await
            }
        }
    }
}

/// Download one file into `dest_path`.
///
/// Returns `Ok(true)` when the file was downloaded, `Ok(false)` when it was
/// already complete on disk. Interrupted downloads are kept in a `.megacli.tmp`
/// file and resumed on the next run (only the missing part is fetched again).
#[allow(clippy::too_many_arguments)]
async fn download_file(
    client: &Client,
    source: &UrlSource,
    file_size: u64,
    file_key: &str,
    dest_path: &Path,
    workers: usize,
    speed_limit: u64,
) -> Result<bool> {
    // Already there?
    if let Ok(meta) = tokio::fs::metadata(dest_path).await {
        if meta.is_file() && meta.len() == file_size {
            return Ok(false);
        }
    }

    if let Some(parent) = dest_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let file_name = dest_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "fichier".to_string());
    let temp_path = dest_path.with_file_name(format!("{}{}", file_name, TEMP_SUFFIX));

    if file_size == 0 {
        tokio::fs::write(dest_path, b"").await?;
        return Ok(true);
    }

    let (aes_key, aes_iv) = derive_file_keys(file_key)?;
    let chunks = build_chunks(file_size);
    let workers = workers.clamp(1, MAX_WORKERS);

    // --- Resume support -----------------------------------------------------
    // The temp file is only ever written at real chunk offsets, so its length
    // tells how many bytes were fully written. A half written chunk is simply
    // re-downloaded from its start.
    let mut on_disk = 0u64;
    if let Ok(meta) = tokio::fs::metadata(&temp_path).await {
        if meta.is_file() && meta.len() <= file_size {
            on_disk = meta.len();
        }
    }

    let (start_index, resume_at) = resume_plan(&chunks, file_size, on_disk);

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&temp_path)
        .await?;
    if file.metadata().await?.len() != resume_at {
        file.set_len(resume_at).await?;
    }

    if resume_at > 0 {
        println!("  Reprise a {}", api::format_bytes(resume_at));
    }

    // --- Progress -----------------------------------------------------------
    let pb = ProgressBar::new(file_size);
    pb.set_style(progress_style());
    pb.set_prefix(short_label(&file_name, 26));
    pb.set_position(resume_at);

    // --- Shared state -------------------------------------------------------
    let output_file = Arc::new(Mutex::new(file));
    let chunks = Arc::new(chunks);
    let next_chunk = Arc::new(AtomicU64::new(start_index as u64));
    let failed = Arc::new(AtomicBool::new(false));
    let throttle = Arc::new(Throttle::new(speed_limit));
    let aes_key = Arc::new(aes_key);
    let aes_iv = Arc::new(aes_iv);

    // Download URLs expire, so the current one is shared and renew on demand.
    let dl_url = Arc::new(RwLock::new(source.fetch(client).await?));
    let url_generation = Arc::new(AtomicU64::new(0));
    let url_refreshes = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..workers {
        let client = client.clone();
        let source = source.clone();
        let chunks = chunks.clone();
        let next_chunk = next_chunk.clone();
        let failed = failed.clone();
        let output_file = output_file.clone();
        let throttle = throttle.clone();
        let aes_key = aes_key.clone();
        let aes_iv = aes_iv.clone();
        let dl_url = dl_url.clone();
        let url_generation = url_generation.clone();
        let url_refreshes = url_refreshes.clone();
        let pb = pb.clone();

        handles.push(tokio::spawn(async move {
            loop {
                if failed.load(Ordering::Relaxed) {
                    break;
                }

                let index = next_chunk.fetch_add(1, Ordering::SeqCst) as usize;
                if index >= chunks.len() {
                    break;
                }

                let (offset, size) = chunks[index];
                let current_url = dl_url.read().await.clone();
                let chunk_url = gen_chunk_url(&current_url, file_size, offset, size);

                let encrypted = match fetch_chunk(&client, &chunk_url, size).await {
                    Ok(data) => data,
                    Err(ChunkError::Expired) => {
                        // Renew the download URL and retry this chunk once.
                        match refresh_url(
                            &client,
                            &source,
                            &dl_url,
                            &url_generation,
                            &url_refreshes,
                        )
                        .await
                        {
                            Ok(fresh_url) => {
                                let retry_url = gen_chunk_url(&fresh_url, file_size, offset, size);
                                match fetch_chunk(&client, &retry_url, size).await {
                                    Ok(data) => data,
                                    Err(e) => {
                                        report_error(&pb, offset, size, &e.to_string());
                                        failed.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                report_error(&pb, offset, size, &e.to_string());
                                failed.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        report_error(&pb, offset, size, &e.to_string());
                        failed.store(true, Ordering::Relaxed);
                        break;
                    }
                };

                // Decrypt the chunk at its own position in the file stream.
                let mut buffer = encrypted;
                let iv = forward_mega_link_key_iv(aes_iv.as_slice(), offset);
                let mut cipher = make_aes_ctr_cipher(aes_key.as_slice(), &iv);
                cipher.apply_keystream(&mut buffer);

                {
                    let mut file = output_file.lock().await;
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
                        report_error(&pb, offset, size, &format!("seek: {}", e));
                        failed.store(true, Ordering::Relaxed);
                        break;
                    }
                    if let Err(e) = file.write_all(&buffer).await {
                        report_error(&pb, offset, size, &format!("ecriture: {}", e));
                        failed.store(true, Ordering::Relaxed);
                        break;
                    }
                }

                pb.inc(size);
                throttle.consume(size).await;
            }
        }));
    }

    for handle in handles {
        if let Err(e) = handle.await {
            failed.store(true, Ordering::Relaxed);
            pb.suspend(|| eprintln!("\n  Tache interrompue: {}", e));
        }
    }

    // Release the file handle (needed on Windows before renaming).
    drop(output_file);

    if failed.load(Ordering::Relaxed) {
        pb.abandon_with_message("interrompu");
        return Err(anyhow!(
            "telechargement interrompu - relance pour reprendre ({})",
            temp_path.display()
        ));
    }

    let written = tokio::fs::metadata(&temp_path).await?.len();
    if written != file_size {
        pb.abandon_with_message("incomplet");
        return Err(anyhow!(
            "fichier incomplet: {} / {}",
            api::format_bytes(written),
            api::format_bytes(file_size)
        ));
    }

    pb.finish_and_clear();

    if dest_path.exists() {
        tokio::fs::remove_file(dest_path).await?;
    }
    tokio::fs::rename(&temp_path, dest_path).await?;

    Ok(true)
}

fn report_error(pb: &ProgressBar, offset: u64, size: u64, message: &str) {
    pb.suspend(|| {
        eprintln!(
            "\n  Erreur chunk {}-{} : {}",
            offset,
            offset + size.saturating_sub(1),
            message
        )
    });
}

/// Get a new download URL, making sure only one worker renews it at a time.
async fn refresh_url(
    client: &Client,
    source: &UrlSource,
    dl_url: &Arc<RwLock<String>>,
    generation: &Arc<AtomicU64>,
    refreshes: &Arc<AtomicU64>,
) -> Result<String, ChunkError> {
    let seen = generation.load(Ordering::SeqCst);
    let mut guard = dl_url.write().await;

    // Somebody else already renewed it while we were waiting for the lock.
    if generation.load(Ordering::SeqCst) != seen {
        return Ok(guard.clone());
    }

    if refreshes.fetch_add(1, Ordering::SeqCst) >= MAX_URL_REFRESH {
        return Err(ChunkError::Fatal(
            "lien de telechargement expire (trop de renouvellements)".to_string(),
        ));
    }

    match source.fetch(client).await {
        Ok(url) => {
            *guard = url.clone();
            generation.fetch_add(1, Ordering::SeqCst);
            Ok(url)
        }
        Err(e) => Err(ChunkError::Transient(format!(
            "renouvellement du lien impossible: {}",
            e
        ))),
    }
}

/// Why a chunk download failed.
enum ChunkError {
    /// HTTP 403: the download URL has expired.
    Expired,
    /// Retrying will not help (quota, missing node, ...).
    Fatal(String),
    /// Worth another try (network hiccup, 5xx, truncated body).
    Transient(String),
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Expired => write!(f, "lien expire (HTTP 403)"),
            ChunkError::Fatal(message) | ChunkError::Transient(message) => {
                write!(f, "{}", message)
            }
        }
    }
}

/// Fetch one chunk, retrying transient errors only.
async fn fetch_chunk(client: &Client, url: &str, expected: u64) -> Result<Vec<u8>, ChunkError> {
    let mut last_error = String::from("erreur inconnue");

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = 1u64 << (attempt.min(4) - 1);
            tokio::time::sleep(Duration::from_secs(backoff.min(8))).await;
        }

        match try_fetch_chunk(client, url, expected).await {
            Ok(data) => return Ok(data),
            Err(ChunkError::Transient(message)) => last_error = message,
            Err(other) => return Err(other),
        }
    }

    Err(ChunkError::Transient(last_error))
}

async fn try_fetch_chunk(
    client: &Client,
    url: &str,
    expected: u64,
) -> Result<Vec<u8>, ChunkError> {
    let response = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| ChunkError::Transient(format!("reseau: {}", e)))?;

    let status = response.status();

    if status.as_u16() == 403 {
        return Err(ChunkError::Expired);
    }
    if status.as_u16() == 509 {
        return Err(ChunkError::Fatal(
            "quota de transfert MEGA atteint (HTTP 509)".to_string(),
        ));
    }
    if !status.is_success() {
        let message = format!("HTTP {}", status.as_u16());
        let transient = status.is_server_error() || status.as_u16() == 429;
        return Err(if transient {
            ChunkError::Transient(message)
        } else {
            ChunkError::Fatal(message)
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| ChunkError::Transient(format!("lecture: {}", e)))?;

    if bytes.len() as u64 != expected {
        return Err(ChunkError::Transient(format!(
            "taille incorrecte: attendu {}, recu {}",
            expected,
            bytes.len()
        )));
    }

    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Where to resume a partially downloaded file: index of the first incomplete
/// chunk and the byte offset to restart from.
fn resume_plan(chunks: &[(u64, u64)], file_size: u64, on_disk: u64) -> (usize, u64) {
    let mut start_index = 0usize;
    while start_index < chunks.len() && chunks[start_index].0 + chunks[start_index].1 <= on_disk {
        start_index += 1;
    }

    let resume_at = if start_index < chunks.len() {
        chunks[start_index].0
    } else {
        file_size
    };

    (start_index, resume_at)
}

/// Split a MEGA file into its (offset, size) chunks.
fn build_chunks(file_size: u64) -> Vec<(u64, u64)> {
    let mut chunks = Vec::new();
    let mut chunk_id = 1u64;

    loop {
        let offset = calculate_chunk_offset(chunk_id, CHUNK_SIZE_MULTI);
        if offset >= file_size {
            break;
        }
        let size = calculate_chunk_size(chunk_id, file_size, offset, CHUNK_SIZE_MULTI);
        if size == 0 {
            break;
        }
        chunks.push((offset, size));
        chunk_id += 1;
    }

    chunks
}

/// AES key + IV used to decrypt a file, from its base64 node key.
fn derive_file_keys(file_key: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let raw = url_base64_decode(file_key)?;
    if raw.len() >= 32 {
        Ok((init_mega_link_key(file_key)?, init_mega_link_key_iv(file_key)?))
    } else if raw.len() == 16 {
        Ok((raw, vec![0u8; 16]))
    } else {
        Err(anyhow!("cle de fichier invalide"))
    }
}

/// Build the relative path of every node of a folder listing.
fn resolve_paths(nodes: &[api::FileNode], folder_id: &str) -> HashMap<String, PathBuf> {
    let by_handle: HashMap<&str, &api::FileNode> =
        nodes.iter().map(|node| (node.handle.as_str(), node)).collect();

    // The shared folder itself is the root: it is either listed under its own
    // handle (usually without a key) or absent from the listing. Its name is
    // never part of a download path.
    let mut paths = HashMap::new();

    for node in nodes {
        if node.handle == folder_id || node.node_type == 2 {
            continue;
        }

        let mut parts: Vec<String> = Vec::new();
        let mut current = node;
        let mut guard = 0usize;

        loop {
            parts.push(sanitize_file_name(&current.name));

            if current.parent.is_empty() || current.parent == folder_id {
                break;
            }

            let parent = match by_handle.get(current.parent.as_str()) {
                Some(&parent) => parent,
                None => break, // top level node: its parent is not in the listing
            };
            if parent.handle == current.handle || parent.handle == folder_id {
                break;
            }
            current = parent;

            guard += 1;
            if guard > 512 {
                // Corrupted listing: fall back to the node name only.
                parts.clear();
                parts.push(sanitize_file_name(&node.name));
                break;
            }
        }

        parts.reverse();
        if parts.is_empty() {
            parts.push(sanitize_file_name(&node.name));
        }

        let path: PathBuf = parts.iter().map(|part| part.as_str()).collect();
        paths.insert(node.handle.clone(), path);
    }

    paths
}

/// Make a MEGA node name safe to use as a single path component.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "sans_nom".to_string()
    } else {
        trimmed.to_string()
    }
}

fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "  {prefix} {bar:32.cyan/blue} {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=>-")
}

fn short_label(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let mut label: String = name.chars().take(max.saturating_sub(1)).collect();
    label.push('~');
    label
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

fn format_speed(bytes_per_sec: f64) -> String {
    format!("{}/s", api::format_bytes(bytes_per_sec as u64))
}

/// Global bandwidth limiter shared by all workers of one file.
struct Throttle {
    bytes_per_sec: f64,
    started: Instant,
    bytes: AtomicU64,
}

impl Throttle {
    fn new(kib_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: kib_per_sec as f64 * 1024.0,
            started: Instant::now(),
            bytes: AtomicU64::new(0),
        }
    }

    /// Sleep as needed so the average speed stays under the configured limit.
    async fn consume(&self, bytes: u64) {
        if self.bytes_per_sec <= 0.0 {
            return;
        }

        let total = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let expected = total as f64 / self.bytes_per_sec;
        let elapsed = self.started.elapsed().as_secs_f64();

        if expected > elapsed {
            let wait = expected - elapsed;
            if wait > 0.001 {
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{build_chunks, derive_file_keys, resume_plan, resolve_paths, sanitize_file_name};
    use crate::api::FileNode;
    use crate::crypto::{forward_mega_link_key_iv, make_aes_ctr_cipher, url_base64_encode};
    use ctr::cipher::StreamCipher;
    use std::path::PathBuf;

    fn node(handle: &str, parent: &str, node_type: i32, name: &str) -> FileNode {
        FileNode {
            handle: handle.to_string(),
            parent: parent.to_string(),
            node_type,
            size: 0,
            name: name.to_string(),
            key: String::new(),
        }
    }

    #[test]
    fn chunks_are_contiguous_and_cover_the_file() {
        let sizes = [
            1u64,
            1024,
            131_071,
            131_072,
            131_073,
            3 * 1024 * 1024,
            (1024 * 1024) * 37 + 12_345,
        ];

        for size in sizes {
            let chunks = build_chunks(size);
            assert!(!chunks.is_empty(), "no chunk for size {}", size);
            assert_eq!(chunks[0].0, 0, "first chunk must start at 0");

            let mut expected_offset = 0u64;
            for (offset, chunk_size) in &chunks {
                assert_eq!(*offset, expected_offset, "gap in chunk plan for {}", size);
                assert!(*chunk_size > 0);
                expected_offset += chunk_size;
            }

            assert_eq!(expected_offset, size, "chunks do not cover the file");
        }
    }

    #[test]
    fn chunk_plan_follows_the_mega_table() {
        let chunks = build_chunks(3 * 1024 * 1024);
        let offsets: Vec<u64> = chunks.iter().map(|(offset, _)| *offset).collect();
        assert_eq!(
            offsets,
            vec![0, 131_072, 393_216, 786_432, 1_310_720, 1_966_080, 2_752_512]
        );

        let sizes: Vec<u64> = chunks.iter().map(|(_, size)| *size).collect();
        assert_eq!(
            sizes,
            vec![131_072, 262_144, 393_216, 524_288, 655_360, 786_432, 393_216]
        );
    }

    #[test]
    fn per_chunk_decryption_matches_a_single_stream() {
        // 32 byte node key (two 16 byte halves XOR-ed together by MEGA).
        let raw_key: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7).wrapping_add(3)).collect();
        let key_b64 = url_base64_encode(&raw_key);
        let (aes_key, aes_iv) = derive_file_keys(&key_b64).unwrap();
        assert_eq!(aes_key.len(), 16);
        assert_eq!(aes_iv.len(), 16);

        let size = 3 * 1024 * 1024 + 12_345u64;
        let plaintext: Vec<u8> = (0..size)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
            .collect();

        // What MEGA stores: the whole file encrypted as one CTR stream.
        let mut ciphertext = plaintext.clone();
        make_aes_ctr_cipher(&aes_key, &aes_iv).apply_keystream(&mut ciphertext);

        // What we download: independent chunks, each decrypted at its offset.
        let mut decrypted = vec![0u8; size as usize];
        for (offset, chunk_size) in build_chunks(size) {
            let iv = forward_mega_link_key_iv(&aes_iv, offset);
            let range = offset as usize..(offset + chunk_size) as usize;
            let mut buffer = ciphertext[range.clone()].to_vec();
            make_aes_ctr_cipher(&aes_key, &iv).apply_keystream(&mut buffer);
            decrypted[range].copy_from_slice(&buffer);
        }

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn keys_are_xor_folded_like_mega_links() {
        let raw_key: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(5).wrapping_add(1)).collect();
        let key_b64 = url_base64_encode(&raw_key);
        let (aes_key, aes_iv) = derive_file_keys(&key_b64).unwrap();

        let expected_key: Vec<u8> = (0..16).map(|i| raw_key[i] ^ raw_key[i + 16]).collect();
        assert_eq!(aes_key, expected_key);
        assert_eq!(aes_iv[..8], raw_key[16..24]);
        assert_eq!(aes_iv[8..], [0u8; 8]);
    }

    #[test]
    fn resume_only_restarts_unfinished_chunks() {
        let chunks = build_chunks(3 * 1024 * 1024);

        assert_eq!(resume_plan(&chunks, 3 * 1024 * 1024, 0), (0, 0));

        // First chunk fully written, second one partial.
        assert_eq!(resume_plan(&chunks, 3 * 1024 * 1024, 131_072 + 1000), (1, 131_072));

        // Everything is on disk: nothing left to do.
        assert_eq!(
            resume_plan(&chunks, 3 * 1024 * 1024, 3 * 1024 * 1024),
            (chunks.len(), 3 * 1024 * 1024)
        );
    }

    #[test]
    fn paths_rebuild_the_folder_tree() {
        let nodes = vec![
            node("ROOT", "", 2, "Yas Sab - Dropshipping Revolution"),
            node("D1", "ROOT", 1, "Module 1"),
            node("D2", "D1", 1, "Sous-dossier"),
            node("F1", "ROOT", 0, "racine.pdf"),
            node("F2", "D1", 0, "video.mp4"),
            node("F3", "D2", 0, "notes.txt"),
            node("F4", "INCONNU", 0, "orphelin.txt"),
        ];

        let paths = resolve_paths(&nodes, "ROOT");

        assert_eq!(paths.get("F1"), Some(&PathBuf::from("racine.pdf")));
        assert_eq!(
            paths.get("F2"),
            Some(&PathBuf::from("Module 1").join("video.mp4"))
        );
        assert_eq!(
            paths.get("F3"),
            Some(&PathBuf::from("Module 1").join("Sous-dossier").join("notes.txt"))
        );
        // Unknown parent: keep the file at the root instead of losing it.
        assert_eq!(paths.get("F4"), Some(&PathBuf::from("orphelin.txt")));
        // The shared folder itself is never part of a path.
        assert_eq!(paths.get("ROOT"), None);
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(sanitize_file_name("Module 1 - Creer sa boutique"), "Module 1 - Creer sa boutique");
        assert_eq!(sanitize_file_name("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_file_name("   "), "sans_nom");
        assert_eq!(sanitize_file_name(".."), "sans_nom");
        assert_eq!(sanitize_file_name("ligne\nbrisee"), "ligne_brisee");
    }

    #[test]
    fn padded_node_keys_are_accepted() {
        let raw_key: Vec<u8> = (0..32u8).collect();
        let padded = format!("{}=", url_base64_encode(&raw_key));
        let (key, iv) = derive_file_keys(&padded).expect("padded key");
        assert_eq!(key.len(), 16);
        assert_eq!(iv.len(), 16);
    }

    #[test]
    fn invalid_node_keys_are_rejected() {
        assert!(derive_file_keys("AAAA").is_err());
        assert!(derive_file_keys("").is_err());
    }
}
