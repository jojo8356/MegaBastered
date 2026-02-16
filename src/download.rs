use anyhow::{Result, anyhow};
use ctr::cipher::StreamCipher;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::api;
use crate::crypto::*;

const CHUNK_SIZE_MULTI: u64 = 20;

/// Download a file from a MEGA link with multi-connection
pub async fn download_from_link(
    client: &Client,
    link: &str,
    dest_dir: &str,
    workers: usize,
    _speed_limit: u64, // KiB/s, 0 = unlimited
) -> Result<()> {
    let link = api::normalize_mega_link(link);

    // Get metadata
    let (filename, file_size, file_key) = api::get_file_metadata(client, &link, None).await?;
    println!("  Fichier: {}", filename);
    println!("  Taille:  {}", api::format_bytes(file_size));

    // Get download URL
    let dl_url = api::get_file_download_url(client, &link, None).await?;

    // Prepare keys
    let aes_key = init_mega_link_key(&file_key)?;
    let aes_iv = init_mega_link_key_iv(&file_key)?;

    // Output file
    let dest_path = Path::new(dest_dir).join(&filename);
    let temp_path = dest_path.with_extension("megacli.tmp");

    let num_workers = workers.min(20).max(1);

    // Calculate all chunks
    let mut chunks = Vec::new();
    let mut chunk_id = 1u64;
    loop {
        let offset = calculate_chunk_offset(chunk_id, CHUNK_SIZE_MULTI);
        if offset >= file_size {
            break;
        }
        let size = calculate_chunk_size(chunk_id, file_size, offset, CHUNK_SIZE_MULTI);
        chunks.push((chunk_id, offset, size));
        chunk_id += 1;
    }

    // Progress bar
    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::with_template(
            "  {bar:40.cyan/blue} {percent}% {bytes}/{total_bytes} [{bytes_per_sec}] ETA {eta}",
        )
        .unwrap()
        .progress_chars("##-"),
    );

    // Pre-allocate output file
    {
        let f = File::create(&temp_path).await?;
        f.set_len(file_size).await?;
    }

    // Shared state
    let chunk_index = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let output_file = Arc::new(Mutex::new(
        OpenOptions::new()
            .write(true)
            .open(&temp_path)
            .await?,
    ));

    let chunks = Arc::new(chunks);
    let dl_url = Arc::new(dl_url);
    let aes_key = Arc::new(aes_key);
    let aes_iv = Arc::new(aes_iv);
    let pb = Arc::new(pb);

    // Spawn workers
    let mut handles = Vec::new();
    for _worker_id in 0..num_workers {
        let chunk_index = chunk_index.clone();
        let failed = failed.clone();
        let output_file = output_file.clone();
        let chunks = chunks.clone();
        let dl_url = dl_url.clone();
        let aes_key = aes_key.clone();
        let aes_iv = aes_iv.clone();
        let pb = pb.clone();
        let client = client.clone();

        let handle = tokio::spawn(async move {
            loop {
                if failed.load(Ordering::Relaxed) {
                    break;
                }

                let idx = chunk_index.fetch_add(1, Ordering::SeqCst) as usize;
                if idx >= chunks.len() {
                    break;
                }

                let (chunk_id, offset, size) = chunks[idx];
                let url = gen_chunk_url(&dl_url, file_size, offset, size);

                let mut retries = 0;
                loop {
                    match download_chunk(&client, &url, size).await {
                        Ok(encrypted_data) => {
                            // Decrypt with AES-CTR
                            let iv = forward_mega_link_key_iv(&aes_iv, offset);
                            let mut cipher = make_aes_ctr_cipher(&aes_key, &iv);
                            let mut decrypted = encrypted_data;
                            cipher.apply_keystream(&mut decrypted);

                            // Write to file at correct offset
                            {
                                let mut file = output_file.lock().await;
                                use tokio::io::AsyncSeekExt;
                                file.seek(std::io::SeekFrom::Start(offset)).await.ok();
                                if let Err(e) = file.write_all(&decrypted).await {
                                    eprintln!("\n  Erreur ecriture chunk {}: {}", chunk_id, e);
                                    failed.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }

                            pb.inc(size);
                            break;
                        }
                        Err(e) => {
                            retries += 1;
                            if retries > 10 {
                                eprintln!("\n  Erreur chunk {} apres {} tentatives: {}", chunk_id, retries, e);
                                failed.store(true, Ordering::Relaxed);
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(retries.min(8))).await;
                        }
                    }
                }
            }
        });
        handles.push(handle);
    }

    // Wait for all workers
    for h in handles {
        h.await?;
    }

    pb.finish_with_message("done");

    if failed.load(Ordering::Relaxed) {
        // Clean up temp file
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(anyhow!("Download echoue"));
    }

    // Rename temp to final
    if dest_path.exists() {
        tokio::fs::remove_file(&dest_path).await?;
    }
    tokio::fs::rename(&temp_path, &dest_path).await?;

    println!("  Sauvegarde: {}", dest_path.display());
    Ok(())
}

async fn download_chunk(client: &Client, url: &str, expected_size: u64) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("HTTP {}", status));
    }

    let bytes = resp.bytes().await?;
    if bytes.len() as u64 != expected_size {
        return Err(anyhow!(
            "Taille incorrecte: attendu {} recu {}",
            expected_size,
            bytes.len()
        ));
    }

    Ok(bytes.to_vec())
}

/// Download all files from a public folder link
pub async fn download_folder(
    client: &Client,
    link: &str,
    dest_dir: &str,
    _workers: usize,
) -> Result<()> {
    let (folder_id, folder_key) = api::extract_folder_id_and_key(link)?;

    println!("  Chargement du contenu du dossier...");
    let nodes = api::get_folder_nodes(client, &folder_id, &folder_key).await?;

    // Build path tree
    let files: Vec<_> = nodes.iter().filter(|n| n.node_type == 0).collect();
    let folders: Vec<_> = nodes.iter().filter(|n| n.node_type == 1 || n.node_type == 2).collect();

    println!("  {} fichiers, {} dossiers trouves", files.len(), folders.len());

    // Create folder structure
    fn build_path(node: &api::FileNode, nodes: &[api::FileNode]) -> String {
        let parent = nodes.iter().find(|n| n.handle == node.parent);
        match parent {
            Some(p) if p.node_type != 2 => format!("{}/{}", build_path(p, nodes), node.name),
            _ => node.name.clone(),
        }
    }

    for folder in &folders {
        if folder.node_type == 2 {
            continue;
        }
        let path = Path::new(dest_dir).join(build_path(folder, &nodes));
        tokio::fs::create_dir_all(&path).await?;
    }

    // Download each file
    let _folder_key_bytes = url_base64_key_decode(&folder_key)?;

    for file_node in &files {
        let rel_path = build_path(file_node, &nodes);
        let full_path = Path::new(dest_dir).join(&rel_path);

        // Create N-link for this file
        // We need the file's decrypted key to build a proper download link
        let _n_link = format!(
            "https://mega.nz/#N!{}!{}###n={}",
            file_node.handle,
            url_base64_encode(&[0; 32]), // placeholder
            folder_id
        );

        println!("\n  -> {}", rel_path);

        // Get download URL using folder context
        let request = format!(r#"[{{"a":"g","g":"1","n":"{}"}}]"#, file_node.handle);
        let res = api::raw_api_request_with_node(client, &request, None, &folder_id).await?;

        if let Some(_dl_url) = res[0]["g"].as_str() {
            // We need the proper key for this file - already decoded in the node name
            // For folder files, the key was already decrypted during get_folder_nodes
            // We need to re-extract it properly

            // For now, download the raw encrypted data and we'll handle key separately
            // This is a simplified version - full implementation would track keys per node
            println!("    Taille: {}", api::format_bytes(file_node.size));

            if file_node.size == 0 {
                tokio::fs::write(&full_path, b"").await?;
                continue;
            }

            // TODO: proper per-file key tracking for folder downloads
            // For now just show that we found the files
            println!("    [Telecharger les fichiers de dossiers necessite un lien direct par fichier]");
        }
    }

    Ok(())
}
