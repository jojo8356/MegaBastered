use anyhow::{anyhow, Result};
use ctr::cipher::StreamCipher;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
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
/// Suffix of the sidecar listing the chunks already on disk.
const PARTS_SUFFIX: &str = ".megacli.parts";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How a transfer should behave.
#[derive(Debug, Clone, Copy)]
pub struct DownloadConfig {
    /// Parallel connections per file (1-20).
    pub workers: usize,
    /// Bandwidth limit in KiB/s, 0 = unlimited.
    pub speed_limit: u64,
    /// When MEGA answers HTTP 509 (transfer quota), wait this long and resume
    /// instead of giving up. Zero means "stop and let the user come back".
    pub quota_wait: Duration,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            workers: 6,
            speed_limit: 0,
            quota_wait: Duration::ZERO,
        }
    }
}

/// Why a file could not be downloaded.
#[derive(Debug)]
pub enum FileError {
    /// MEGA's transfer quota (HTTP 509) is exhausted for this IP address.
    Quota,
    /// Anything else.
    Other(String),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::Quota => write!(f, "quota de transfert MEGA atteint (HTTP 509)"),
            FileError::Other(message) => write!(f, "{}", message),
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Download a file (or a whole folder) from a public MEGA link.
pub async fn download_from_link(
    client: &Client,
    link: &str,
    dest_dir: &str,
    config: DownloadConfig,
) -> Result<()> {
    if api::is_folder_link(link) {
        return download_folder(client, link, dest_dir, config)
            .await
            .map(|_| ());
    }

    let link = api::normalize_mega_link(link);
    let (filename, file_size, file_key) = api::get_file_metadata(client, &link, None).await?;

    println!("  Fichier: {}", filename);
    println!("  Taille:  {}", api::format_bytes(file_size));

    let dest = Path::new(dest_dir).join(sanitize_file_name(&filename));
    let source = UrlSource::Link(link);

    match download_file(client, &source, file_size, &file_key, &dest, config).await {
        Ok(true) => {
            println!("\n  Sauvegarde: {}", dest.display());
            Ok(())
        }
        Ok(false) => {
            println!("\n  Deja present: {}", dest.display());
            Ok(())
        }
        Err(FileError::Quota) => Err(anyhow!(
            "quota de transfert MEGA atteint (HTTP 509) - relance le meme lien plus tard, la reprise est automatique"
        )),
        Err(FileError::Other(message)) => Err(anyhow!(message)),
    }
}

/// One entry of a folder listing.
pub struct FolderEntry {
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

/// Result of a folder download.
pub struct FolderReport {
    pub downloaded: usize,
    pub skipped: usize,
    pub failed: usize,
    /// True when the run stopped early because MEGA's quota was exhausted.
    pub quota_exhausted: bool,
}

/// List a public folder without downloading anything.
pub async fn list_folder(client: &Client, link: &str) -> Result<Vec<FolderEntry>> {
    let (folder_id, folder_key) = api::extract_folder_id_and_key(link)?;
    let nodes = api::get_folder_nodes(client, &folder_id, &folder_key).await?;
    let paths = resolve_paths(&nodes, &folder_id);

    let mut entries: Vec<FolderEntry> = nodes
        .iter()
        .filter(|node| node.handle != folder_id)
        .map(|node| FolderEntry {
            path: paths
                .get(&node.handle)
                .cloned()
                .unwrap_or_else(|| PathBuf::from(sanitize_file_name(&node.name))),
            size: node.size,
            is_dir: node.is_folder(),
        })
        .collect();

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Download every file of a public MEGA folder, recreating the directory tree.
pub async fn download_folder(
    client: &Client,
    link: &str,
    dest_dir: &str,
    config: DownloadConfig,
) -> Result<FolderReport> {
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
    println!(
        "  Rappel: le quota MEGA gratuit est d'environ 5 Go par fenetre de 6 h (par adresse IP)."
    );

    if files.is_empty() {
        println!("\n  Aucun fichier a telecharger (dossiers vides recrees).");
        return Ok(FolderReport {
            downloaded: 0,
            skipped: 0,
            failed: 0,
            quota_exhausted: false,
        });
    }

    // --- Fast pre-check -----------------------------------------------------
    // A single concurrent metadata pass tells what is already on disk, so a
    // finished file costs one stat call and never triggers a download URL.
    println!("\n  Verification des fichiers deja presents...");
    let targets: Vec<(u64, PathBuf)> = files
        .iter()
        .map(|(node, relative)| (node.size, base.join(relative)))
        .collect();
    let states = scan_disk(&targets).await;

    let mut pending: Vec<usize> = Vec::new();
    let mut skipped_bytes = 0u64;
    let mut cleaned = 0usize;

    for (index, state) in states.iter().enumerate() {
        match state {
            DiskState::Complete => {
                skipped_bytes += files[index].0.size;
                if cleanup_sidecars(&targets[index].1).await {
                    cleaned += 1;
                }
            }
            DiskState::Partial(_) | DiskState::Missing => pending.push(index),
        }
    }

    let complete = states.len() - pending.len();
    let pending_bytes: u64 = pending.iter().map(|index| files[*index].0.size).sum();

    println!(
        "  {} fichier(s) deja complet(s) ({}) ignore(s)",
        complete,
        api::format_bytes(skipped_bytes)
    );
    println!(
        "  {} fichier(s) a telecharger ou reprendre ({})",
        pending.len(),
        api::format_bytes(pending_bytes)
    );
    if cleaned > 0 {
        println!(
            "  {} fichier(s) temporaire(s) obsolete(s) nettoye(s)",
            cleaned
        );
    }

    if pending.is_empty() {
        println!("\n  Tout est deja telecharge, rien a faire.");
        return Ok(FolderReport {
            downloaded: 0,
            skipped: complete,
            failed: 0,
            quota_exhausted: false,
        });
    }

    let sessions = pending_bytes.div_ceil(5 * 1024 * 1024 * 1024).max(1);
    if pending_bytes > 5 * 1024 * 1024 * 1024 {
        println!(
            "  A ~5 Go par fenetre de 6 h (quota gratuit), prevoir ~{} session(s)",
            sessions
        );
    }

    let started = Instant::now();
    let mut report = FolderReport {
        downloaded: 0,
        skipped: complete,
        failed: 0,
        quota_exhausted: false,
    };
    let mut done_bytes = 0u64;
    let mut failures: Vec<(String, String)> = Vec::new();

    for (position, index) in pending.iter().enumerate() {
        let (node, relative) = &files[*index];
        let dest = base.join(relative);
        println!(
            "\n  [{}/{}] {} ({})",
            position + 1,
            pending.len(),
            relative.display(),
            api::format_bytes(node.size)
        );

        if node.key.is_empty() {
            println!("  Echec: cle de dechiffrement indisponible");
            report.failed += 1;
            failures.push((relative.display().to_string(), "cle indisponible".to_string()));
            continue;
        }

        let source = UrlSource::FolderNode {
            folder_id: folder_id.clone(),
            handle: node.handle.clone(),
        };

        match download_file(client, &source, node.size, &node.key, &dest, config).await {
            Ok(true) => {
                report.downloaded += 1;
                done_bytes += node.size;
            }
            Ok(false) => {
                // Completed by another run in the meantime.
                report.skipped += 1;
                skipped_bytes += node.size;
                println!("  Deja present, ignore");
            }
            Err(FileError::Quota) => {
                // MEGA blocks the whole IP address, not just this file: no
                // point hammering the remaining files.
                report.failed += 1;
                report.quota_exhausted = true;
                failures.push((relative.display().to_string(), FileError::Quota.to_string()));
                println!(
                    "\n  Quota MEGA atteint (509). Arret du dossier: les {} fichier(s) restant(s) seront repris en relancant la meme commande.",
                    pending.len() - position - 1
                );
                break;
            }
            Err(FileError::Other(message)) => {
                report.failed += 1;
                failures.push((relative.display().to_string(), message));
            }
        }
    }

    // --- Recap --------------------------------------------------------------
    let elapsed = started.elapsed();

    println!("\n  ------------------------------------------------");
    println!(
        "  Termine en {} - {} telecharge(s), {} deja present(s), {} en echec",
        format_duration(elapsed),
        report.downloaded,
        report.skipped,
        report.failed
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
    }

    if report.quota_exhausted {
        let remaining: u64 = files
            .iter()
            .skip(report.downloaded + report.skipped + report.failed)
            .map(|(node, _)| node.size)
            .sum();
        println!(
            "\n  Quota MEGA epuise (HTTP 509) - environ 5 Go par fenetre glissante de 6 h, par IP."
        );
        println!(
            "  Il reste {} a recuperer. Relance la meme commande dans quelques heures: les fichiers deja complets sont ignores et les autres reprennent a l'octet pres.",
            api::format_bytes(remaining)
        );
    } else if !failures.is_empty() {
        println!("\n  Relance le meme lien pour reprendre les fichiers manquants.");
    }

    Ok(report)
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

/// State shared by the workers of one file.
struct Shared {
    chunks: Vec<(u64, u64)>,
    /// One flag per chunk: written to disk and recorded in the sidecar.
    completed: Vec<AtomicBool>,
    /// Chunks left to fetch in the current round.
    queue: StdMutex<VecDeque<usize>>,
    /// Set when MEGA answers HTTP 509.
    quota: AtomicBool,
    /// Set when the file cannot be completed (network, HTTP error, ...).
    failure: StdMutex<Option<String>>,
    /// Temp file (taken out of the slot to close it before renaming).
    file: Mutex<Option<tokio::fs::File>>,
    /// Sidecar listing the completed chunk offsets.
    parts: Mutex<Option<tokio::fs::File>>,
    dl_url: RwLock<String>,
    url_generation: AtomicU64,
    url_refreshes: AtomicU64,
    throttle: Throttle,
    progress: ProgressBar,
    fetched: AtomicU64,
}

impl Shared {
    fn fail(&self, message: impl Into<String>) {
        let mut slot = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(message.into());
        }
    }

    fn failed(&self) -> bool {
        match self.failure.lock() {
            Ok(slot) => slot.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    fn remaining(&self) -> Vec<usize> {
        self.completed
            .iter()
            .enumerate()
            .filter(|(_, done)| !done.load(Ordering::SeqCst))
            .map(|(index, _)| index)
            .collect()
    }
}

/// Download one file into `dest_path`.
///
/// Returns `Ok(true)` when the file was downloaded, `Ok(false)` when it was
/// already complete on disk. Progress is tracked per chunk in a `.megacli.parts`
/// sidecar: an interrupted transfer resumes exactly where it stopped, and a
/// partial file is never reported as complete.
async fn download_file(
    client: &Client,
    source: &UrlSource,
    file_size: u64,
    file_key: &str,
    dest_path: &Path,
    config: DownloadConfig,
) -> Result<bool, FileError> {
    let fail = |message: String| -> Result<bool, FileError> { Err(FileError::Other(message)) };

    // Already there?
    if let Ok(meta) = tokio::fs::metadata(dest_path).await {
        if meta.is_file() && meta.len() == file_size {
            return Ok(false);
        }
    }

    if let Some(parent) = dest_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| FileError::Other(format!("creation du dossier: {}", e)))?;
    }

    let file_name = dest_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "fichier".to_string());
    let temp_path = dest_path.with_file_name(format!("{}{}", file_name, TEMP_SUFFIX));
    let parts_path = dest_path.with_file_name(format!("{}{}", file_name, PARTS_SUFFIX));

    if file_size == 0 {
        tokio::fs::write(dest_path, b"")
            .await
            .map_err(|e| FileError::Other(format!("ecriture: {}", e)))?;
        return Ok(true);
    }

    let (aes_key, aes_iv) = derive_file_keys(file_key).map_err(|e| FileError::Other(e.to_string()))?;
    let aes_key = Arc::new(aes_key);
    let aes_iv = Arc::new(aes_iv);
    let chunks = build_chunks(file_size);
    let workers = config.workers.clamp(1, MAX_WORKERS);

    // --- Resume -------------------------------------------------------------
    let mut completed_offsets = load_parts(&parts_path).await;
    let temp_exists = tokio::fs::metadata(&temp_path)
        .await
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false);

    if !temp_exists && !completed_offsets.is_empty() {
        // Sidecar without data: start over.
        completed_offsets.clear();
        let _ = tokio::fs::remove_file(&parts_path).await;
    } else if temp_exists && completed_offsets.is_empty() {
        // A temp file left by an older version (no sidecar): we cannot trust
        // its content, so restart this file cleanly.
        println!("  Ancien fichier temporaire ignore, reprise depuis zero");
        let _ = tokio::fs::remove_file(&temp_path).await;
    }

    let known: HashSet<u64> = chunks.iter().map(|(offset, _)| *offset).collect();
    completed_offsets.retain(|offset| known.contains(offset));

    let temp = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&temp_path)
        .await
        .map_err(|e| FileError::Other(format!("ouverture du fichier temporaire: {}", e)))?;

    if temp
        .metadata()
        .await
        .map(|meta| meta.len() > file_size)
        .unwrap_or(false)
    {
        temp.set_len(0)
            .await
            .map_err(|e| FileError::Other(format!("troncature: {}", e)))?;
        completed_offsets.clear();
        let _ = tokio::fs::remove_file(&parts_path).await;
    }

    let parts = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&parts_path)
        .await
        .map_err(|e| FileError::Other(format!("ouverture de l'index de reprise: {}", e)))?;

    let mut completed: Vec<AtomicBool> = Vec::with_capacity(chunks.len());
    let mut done_bytes = 0u64;
    for (offset, size) in &chunks {
        let done = completed_offsets.contains(offset);
        if done {
            done_bytes += size;
        }
        completed.push(AtomicBool::new(done));
    }

    let remaining: VecDeque<usize> = chunks
        .iter()
        .enumerate()
        .filter(|(index, _)| !completed[*index].load(Ordering::SeqCst))
        .map(|(index, _)| index)
        .collect();

    if done_bytes > 0 {
        println!(
            "  Reprise: {} deja sur le disque ({} au total)",
            api::format_bytes(done_bytes),
            api::format_bytes(file_size)
        );
    }

    let progress = new_progress(file_size, &file_name, done_bytes);
    let dl_url = source
        .fetch(client)
        .await
        .map_err(|e| classify_api_error(&e))?;

    let shared = Arc::new(Shared {
        chunks: chunks.clone(),
        completed,
        queue: StdMutex::new(remaining),
        quota: AtomicBool::new(false),
        failure: StdMutex::new(None),
        file: Mutex::new(Some(temp)),
        parts: Mutex::new(Some(parts)),
        dl_url: RwLock::new(dl_url),
        url_generation: AtomicU64::new(0),
        url_refreshes: AtomicU64::new(0),
        throttle: Throttle::new(config.speed_limit),
        progress: progress.clone(),
        fetched: AtomicU64::new(done_bytes),
    });

    // --- Rounds -------------------------------------------------------------
    // A round runs every remaining chunk once. If MEGA's quota interrupts it we
    // either wait (when asked to) or stop, keeping what is already on disk.
    loop {
        // Refill the queue with whatever is still missing (a quota hit in the
        // previous round leaves the chunk it was fetching out of the queue).
        {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.clear();
            queue.extend(shared.remaining());
        }

        run_workers(client, source, file_size, &aes_key, &aes_iv, workers, &shared).await;

        if shared.remaining().is_empty() {
            break;
        }

        if shared.quota.load(Ordering::SeqCst) {
            if config.quota_wait.is_zero() {
                close_handles(&shared).await;
                progress.abandon_with_message("quota MEGA atteint");
                return Err(FileError::Quota);
            }

            println!(
                "  Quota MEGA atteint (509). Pause de {} avant reprise (Ctrl+C pour arreter, la reprise reste possible).",
                format_duration(config.quota_wait)
            );
            tokio::time::sleep(config.quota_wait).await;

            // New URL + fresh round.
            match source.fetch(client).await {
                Ok(url) => {
                    *shared.dl_url.write().await = url;
                    shared.quota.store(false, Ordering::SeqCst);
                }
                Err(e) => {
                    if is_quota_error(&e) {
                        continue; // still over quota: wait again
                    }
                    close_handles(&shared).await;
                    progress.abandon_with_message("erreur API");
                    return Err(FileError::Other(e.to_string()));
                }
            }
        } else if shared.failed() {
            let reason = match shared.failure.lock() {
                Ok(slot) => slot.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            close_handles(&shared).await;
            progress.abandon_with_message("interrompu");
            return Err(FileError::Other(
                reason.unwrap_or_else(|| "telechargement interrompu".to_string()),
            ));
        } else {
            // Round ended without any explicit error but chunks are missing.
            close_handles(&shared).await;
            progress.abandon_with_message("incomplet");
            return Err(FileError::Other(
                "chunks manquants apres plusieurs tentatives".to_string(),
            ));
        }

    }

    // --- Finalize -----------------------------------------------------------
    let written = {
        let guard = shared.file.lock().await;
        match guard.as_ref() {
            None => 0,
            Some(file) => file.metadata().await.map(|meta| meta.len()).unwrap_or(0),
        }
    };
    close_handles(&shared).await;

    if written != file_size {
        progress.abandon_with_message("incomplet");
        return fail(format!(
            "fichier incomplet: {} / {}",
            api::format_bytes(written),
            api::format_bytes(file_size)
        ));
    }

    if dest_path.exists() {
        tokio::fs::remove_file(dest_path)
            .await
            .map_err(|e| FileError::Other(format!("suppression de l'ancien fichier: {}", e)))?;
    }
    tokio::fs::rename(&temp_path, dest_path)
        .await
        .map_err(|e| FileError::Other(format!("renommage final: {}", e)))?;
    let _ = tokio::fs::remove_file(&parts_path).await;

    progress.finish_and_clear();
    Ok(true)
}

/// Run one round: every worker pulls chunks from the queue until it is empty.
async fn run_workers(
    client: &Client,
    source: &UrlSource,
    file_size: u64,
    aes_key: &Arc<Vec<u8>>,
    aes_iv: &Arc<Vec<u8>>,
    workers: usize,
    shared: &Arc<Shared>,
) {
    let mut handles = Vec::new();

    for _ in 0..workers {
        let shared = shared.clone();
        let client = client.clone();
        let source = source.clone();
        let aes_key = aes_key.clone();
        let aes_iv = aes_iv.clone();

        handles.push(tokio::spawn(async move {
            loop {
                if shared.quota.load(Ordering::SeqCst) || shared.failed() {
                    break;
                }

                let index = {
                    let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                    queue.pop_front()
                };
                let index = match index {
                    Some(index) => index,
                    None => break,
                };

                let (offset, size) = shared.chunks[index];
                let url = shared.dl_url.read().await.clone();
                let chunk_url = gen_chunk_url(&url, file_size, offset, size);

                let encrypted = match fetch_chunk(&client, &chunk_url, size).await {
                    Ok(data) => data,
                    Err(ChunkError::Quota) => {
                        shared.quota.store(true, Ordering::SeqCst);
                        break;
                    }
                    Err(ChunkError::Expired) => {
                        match refresh_url(&client, &source, &shared).await {
                            Ok(fresh) => {
                                let retry = gen_chunk_url(&fresh, file_size, offset, size);
                                match fetch_chunk(&client, &retry, size).await {
                                    Ok(data) => data,
                                    Err(ChunkError::Quota) => {
                                        shared.quota.store(true, Ordering::SeqCst);
                                        break;
                                    }
                                    Err(e) => {
                                        shared.fail(e.to_string());
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                if matches!(e, ChunkError::Quota) {
                                    shared.quota.store(true, Ordering::SeqCst);
                                } else {
                                    shared.fail(e.to_string());
                                }
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        shared.fail(e.to_string());
                        break;
                    }
                };

                // Decrypt the chunk at its own position in the file stream.
                let mut buffer = encrypted;
                let iv = forward_mega_link_key_iv(aes_iv.as_slice(), offset);
                make_aes_ctr_cipher(aes_key.as_slice(), &iv).apply_keystream(&mut buffer);

                {
                    let mut guard = shared.file.lock().await;
                    let Some(file) = guard.as_mut() else {
                        break;
                    };
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
                        shared.fail(format!("seek: {}", e));
                        break;
                    }
                    if let Err(e) = file.write_all(&buffer).await {
                        shared.fail(format!("ecriture: {}", e));
                        break;
                    }
                }

                // Record the chunk only once its bytes are really on disk.
                shared.completed[index].store(true, Ordering::SeqCst);
                {
                    let mut guard = shared.parts.lock().await;
                    if let Some(parts) = guard.as_mut() {
                        if parts
                            .write_all(format!("{}\n", offset).as_bytes())
                            .await
                            .is_ok()
                        {
                            let _ = parts.flush().await;
                        }
                    }
                }

                shared.fetched.fetch_add(size, Ordering::Relaxed);
                shared.progress.inc(size);
                shared.throttle.consume(size).await;
            }
        }));
    }

    for handle in handles {
        if let Err(e) = handle.await {
            shared.fail(format!("tache interrompue: {}", e));
        }
    }
}

/// Close the temp file and the sidecar (needed before renaming on Windows).
async fn close_handles(shared: &Arc<Shared>) {
    let _ = shared.file.lock().await.take();
    let _ = shared.parts.lock().await.take();
}

/// Get a new download URL, making sure only one worker renews it at a time.
async fn refresh_url(
    client: &Client,
    source: &UrlSource,
    shared: &Arc<Shared>,
) -> Result<String, ChunkError> {
    let seen = shared.url_generation.load(Ordering::SeqCst);
    let mut guard = shared.dl_url.write().await;

    // Somebody else already renewed it while we were waiting for the lock.
    if shared.url_generation.load(Ordering::SeqCst) != seen {
        return Ok(guard.clone());
    }

    if shared.url_refreshes.fetch_add(1, Ordering::SeqCst) >= MAX_URL_REFRESH {
        return Err(ChunkError::Fatal(
            "lien de telechargement expire (trop de renouvellements)".to_string(),
        ));
    }

    match source.fetch(client).await {
        Ok(url) => {
            *guard = url.clone();
            shared.url_generation.fetch_add(1, Ordering::SeqCst);
            Ok(url)
        }
        Err(e) => {
            if is_quota_error(&e) {
                return Err(ChunkError::Quota);
            }
            Err(ChunkError::Transient(format!(
                "renouvellement du lien impossible: {}",
                e
            )))
        }
    }
}

/// Why a chunk download failed.
enum ChunkError {
    /// HTTP 403: the download URL has expired.
    Expired,
    /// HTTP 509: MEGA's transfer quota is exhausted for this IP.
    Quota,
    /// Retrying on this URL will not help.
    Fatal(String),
    /// Worth another try (network hiccup, 5xx, truncated body).
    Transient(String),
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Expired => write!(f, "lien expire (HTTP 403)"),
            ChunkError::Quota => write!(f, "quota de transfert MEGA atteint (HTTP 509)"),
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
            tokio::time::sleep(Duration::from_millis((backoff * 500).min(8_000))).await;
        }

        match try_fetch_chunk(client, url, expected).await {
            Ok(data) => return Ok(data),
            Err(ChunkError::Transient(message)) => last_error = message,
            Err(other) => return Err(other),
        }
    }

    Err(ChunkError::Transient(last_error))
}

async fn try_fetch_chunk(client: &Client, url: &str, expected: u64) -> Result<Vec<u8>, ChunkError> {
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
        return Err(ChunkError::Quota);
    }
    if status.as_u16() == 429 {
        return Err(ChunkError::Transient("HTTP 429 (trop de requetes)".to_string()));
    }
    if !status.is_success() {
        let message = format!("HTTP {}", status.as_u16());
        return Err(if status.is_server_error() {
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

/// What a destination file looks like on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskState {
    /// File present with the expected size.
    Complete,
    /// File present but not finished (bytes already on disk).
    Partial(u64),
    /// Nothing on disk yet.
    Missing,
}

/// Classify a destination file from its size (if any) and the expected size.
pub fn classify_disk(len: Option<u64>, expected: u64) -> DiskState {
    match len {
        Some(len) if len == expected => DiskState::Complete,
        Some(len) if len > 0 => DiskState::Partial(len),
        _ => DiskState::Missing,
    }
}

/// Stat every target file concurrently.
async fn scan_disk(targets: &[(u64, PathBuf)]) -> Vec<DiskState> {
    let mut states = Vec::with_capacity(targets.len());

    for chunk in targets.chunks(128) {
        let futures = chunk.iter().map(|(size, path)| async move {
            let len = tokio::fs::metadata(path)
                .await
                .ok()
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len());
            classify_disk(len, *size)
        });
        states.extend(futures::future::join_all(futures).await);
    }

    states
}

/// Remove the temp file and the resume index of a finished download.
/// Returns true when something was actually deleted.
async fn cleanup_sidecars(dest: &Path) -> bool {
    let Some(name) = dest.file_name().map(|name| name.to_string_lossy().to_string()) else {
        return false;
    };

    let mut removed = false;
    for suffix in [TEMP_SUFFIX, PARTS_SUFFIX] {
        let leftover = dest.with_file_name(format!("{}{}", name, suffix));
        if leftover.exists() && tokio::fs::remove_file(&leftover).await.is_ok() {
            removed = true;
        }
    }
    removed
}

/// Read the sidecar of completed chunk offsets.
async fn load_parts(path: &Path) -> HashSet<u64> {
    let mut offsets = HashSet::new();
    if let Ok(content) = tokio::fs::read_to_string(path).await {
        for line in content.lines() {
            if let Ok(offset) = line.trim().parse::<u64>() {
                offsets.insert(offset);
            }
        }
    }
    offsets
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

/// Turn an API error into the right file error.
fn classify_api_error(error: &anyhow::Error) -> FileError {
    if is_quota_error(error) {
        FileError::Quota
    } else {
        FileError::Other(error.to_string())
    }
}

/// MEGA reports an exhausted transfer quota with `-17` or HTTP 509.
fn is_quota_error(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    text.contains("-17") || text.contains("509") || text.contains("quota")
}

/// Build the relative path of every node of a folder listing.
fn resolve_paths(nodes: &[api::FileNode], folder_id: &str) -> HashMap<String, PathBuf> {
    let by_handle: HashMap<&str, &api::FileNode> =
        nodes.iter().map(|node| (node.handle.as_str(), node)).collect();

    // The shared folder itself is the root: it is either listed under its own
    // handle or absent from the listing. `folder_id` is our conceptual root.
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

fn new_progress(total: u64, label: &str, position: u64) -> ProgressBar {
    if !std::io::stdout().is_terminal() {
        // Piped output: a bar would just spam the log.
        let pb = ProgressBar::hidden();
        pb.set_length(total);
        pb.set_position(position);
        return pb;
    }

    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "  {prefix} {bar:32.cyan/blue} {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-"),
    );
    pb.set_prefix(short_label(label, 26));
    pb.set_position(position);
    pb
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
    use super::*;
    use crate::api::FileNode;

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
        let raw_key: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7).wrapping_add(3)).collect();
        let key_b64 = url_base64_encode(&raw_key);
        let (aes_key, aes_iv) = derive_file_keys(&key_b64).unwrap();

        let size = 3 * 1024 * 1024 + 12_345u64;
        let plaintext: Vec<u8> = (0..size)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
            .collect();

        let mut ciphertext = plaintext.clone();
        make_aes_ctr_cipher(&aes_key, &aes_iv).apply_keystream(&mut ciphertext);

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

    #[tokio::test]
    async fn parts_sidecar_round_trips() {
        let dir = std::env::temp_dir().join(format!("megacli-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("x.megacli.parts");

        let mut offsets = HashSet::new();
        offsets.insert(0u64);
        offsets.insert(131_072u64);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .unwrap();
        for offset in &offsets {
            file.write_all(format!("{}\n", offset).as_bytes())
                .await
                .unwrap();
        }
        file.flush().await.unwrap();
        drop(file);

        assert_eq!(load_parts(&path).await, offsets);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn missing_parts_file_means_no_progress() {
        let path = std::env::temp_dir().join("megacli-does-not-exist.megacli.parts");
        let _ = tokio::fs::remove_file(&path).await;
        assert!(load_parts(&path).await.is_empty());
    }

    #[test]
    fn completed_offsets_are_filtered_against_the_plan() {
        // A sidecar written for another file size must not be trusted.
        let chunks = build_chunks(1024);
        let plan: HashSet<u64> = chunks.iter().map(|(offset, _)| *offset).collect();

        let mut loaded: HashSet<u64> = HashSet::new();
        loaded.insert(0);
        loaded.insert(999_999);

        loaded.retain(|offset| plan.contains(offset));
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(&0));
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
        assert_eq!(paths.get("F4"), Some(&PathBuf::from("orphelin.txt")));
        assert_eq!(paths.get("ROOT"), None);
    }

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(
            sanitize_file_name("Module 1 - Creer sa boutique"),
            "Module 1 - Creer sa boutique"
        );

        // Every character Windows refuses in a file name becomes "_".
        let dirty = format!("a/b{c}d:e*f?g", c = '\\');
        let dirty = format!("{dirty}\"h<i>j|k");
        assert_eq!(sanitize_file_name(&dirty), "a_b_d_e_f_g_h_i_j_k");

        assert_eq!(sanitize_file_name("   "), "sans_nom");
        assert_eq!(sanitize_file_name(".."), "sans_nom");

        let newline = format!("ligne{}brisee", '\n');
        assert_eq!(sanitize_file_name(&newline), "ligne_brisee");
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

    #[test]
    fn quota_errors_are_recognised() {
        let error = anyhow!("MEGA API error -17 (quota de transfert depasse)");
        assert!(is_quota_error(&error));
        assert!(matches!(classify_api_error(&error), FileError::Quota));

        let other = anyhow!("MEGA API error -9 (element introuvable)");
        assert!(!is_quota_error(&other));
        assert!(matches!(classify_api_error(&other), FileError::Other(_)));
    }

    #[test]
    fn disk_state_classification() {
        assert_eq!(classify_disk(None, 100), DiskState::Missing);
        assert_eq!(classify_disk(Some(0), 100), DiskState::Missing);
        assert_eq!(classify_disk(Some(100), 100), DiskState::Complete);
        assert_eq!(classify_disk(Some(40), 100), DiskState::Partial(40));
        // Bigger than expected: the file is wrong, it will be downloaded again.
        assert_eq!(classify_disk(Some(200), 100), DiskState::Partial(200));
        // Empty files are legitimately complete at zero byte.
        assert_eq!(classify_disk(Some(0), 0), DiskState::Complete);
    }

    #[tokio::test]
    async fn scan_reports_each_target() {
        let dir = std::env::temp_dir().join(format!("megacli-scan-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let done = dir.join("done.bin");
        let partial = dir.join("partial.bin");
        let missing = dir.join("missing.bin");
        tokio::fs::write(&done, vec![0u8; 8]).await.unwrap();
        tokio::fs::write(&partial, vec![0u8; 3]).await.unwrap();

        let targets = vec![
            (8u64, done.clone()),
            (8, partial.clone()),
            (8, missing.clone()),
        ];
        let states = scan_disk(&targets).await;

        assert_eq!(states[0], DiskState::Complete);
        assert_eq!(states[1], DiskState::Partial(3));
        assert_eq!(states[2], DiskState::Missing);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn finished_files_lose_their_leftovers() {
        let dir = std::env::temp_dir().join(format!("megacli-clean-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let dest = dir.join("video.mkv");
        let tmp = dir.join(format!("video.mkv{}", TEMP_SUFFIX));
        let parts = dir.join(format!("video.mkv{}", PARTS_SUFFIX));
        for path in [&dest, &tmp, &parts] {
            tokio::fs::write(path, b"x").await.unwrap();
        }

        assert!(cleanup_sidecars(&dest).await);
        assert!(dest.exists(), "the finished file must be kept");
        assert!(!tmp.exists());
        assert!(!parts.exists());

        // Nothing left to clean.
        assert!(!cleanup_sidecars(&dest).await);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
