use anyhow::{Result, anyhow};
use indicatif::{ProgressBar, ProgressStyle};
use rand::Rng;
use reqwest::Client;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::api::{self, MegaSession};
use crate::crypto::*;

const CHUNK_SIZE_MULTI: u64 = 20;

/// Upload a file to MEGA account
pub async fn upload_file(
    client: &Client,
    session: &MegaSession,
    local_path: &str,
    remote_parent: &str,
) -> Result<String> {
    let path = Path::new(local_path);
    if !path.exists() {
        return Err(anyhow!("Fichier introuvable: {}", local_path));
    }

    let filename = path
        .file_name()
        .ok_or(anyhow!("Nom de fichier invalide"))?
        .to_string_lossy();
    let file_size = tokio::fs::metadata(path).await?.len();

    println!("  Fichier: {}", filename);
    println!("  Taille:  {}", api::format_bytes(file_size));

    // Generate random upload key (6 i32 = 24 bytes)
    let mut rng = rand::thread_rng();
    let ul_key: [i32; 6] = [
        rng.gen(),
        rng.gen(),
        rng.gen(),
        rng.gen(),
        rng.gen(),
        rng.gen(),
    ];

    // Derive AES key and IV from upload key
    let aes_key = i32a_to_bin(&[ul_key[0] ^ ul_key[4], ul_key[1] ^ ul_key[5], ul_key[2], ul_key[3]]);
    let aes_iv_parts = [ul_key[4], ul_key[5], 0i32, 0i32];
    let aes_iv = i32a_to_bin(&aes_iv_parts);

    // Get upload URL
    let ul_url = api::init_upload(client, session, file_size).await?;

    // Progress bar
    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::with_template(
            "  {bar:40.green/black} {percent}% {bytes}/{total_bytes} [{bytes_per_sec}] ETA {eta}",
        )
        .unwrap()
        .progress_chars("##-"),
    );

    // Read entire file and encrypt with AES-CTR
    let mut file = File::open(path).await?;
    let mut plaintext = Vec::with_capacity(file_size as usize);
    file.read_to_end(&mut plaintext).await?;

    let mut cipher = make_aes_ctr_cipher(&aes_key, &aes_iv);
    let mut encrypted = plaintext.clone();
    ctr::cipher::StreamCipher::apply_keystream(&mut cipher, &mut encrypted);

    // Calculate MAC (simplified - XOR of chunk MACs)
    let mut file_mac = [0i32; 4];
    let mut chunk_id = 1u64;
    loop {
        let offset = calculate_chunk_offset(chunk_id, CHUNK_SIZE_MULTI);
        if offset >= file_size {
            break;
        }
        let size = calculate_chunk_size(chunk_id, file_size, offset, CHUNK_SIZE_MULTI);

        // Calculate chunk MAC using AES-CBC
        let chunk_data = &plaintext[offset as usize..(offset + size) as usize];
        let mut mac_iv = [ul_key[4], ul_key[5], ul_key[4], ul_key[5]];
        let _mac_key = i32a_to_bin(&[ul_key[0] ^ ul_key[4], ul_key[1] ^ ul_key[5], ul_key[2], ul_key[3]]);

        // Process chunk in 16-byte blocks
        for block in chunk_data.chunks(16) {
            let mut padded = [0u8; 16];
            padded[..block.len()].copy_from_slice(block);
            let block_i32 = bin_to_i32a(&padded);
            mac_iv[0] ^= block_i32[0];
            mac_iv[1] ^= block_i32[1];
            mac_iv[2] ^= block_i32[2];
            mac_iv[3] ^= block_i32[3];
            let iv_zero = [0i32; 4];
            mac_iv = aes_cbc_encrypt_i32a(&mac_iv, &[ul_key[0] ^ ul_key[4], ul_key[1] ^ ul_key[5], ul_key[2], ul_key[3]], &iv_zero)?;
        }

        file_mac[0] ^= mac_iv[0];
        file_mac[1] ^= mac_iv[1];
        file_mac[2] ^= mac_iv[2];
        file_mac[3] ^= mac_iv[3];

        chunk_id += 1;
    }

    // Upload chunks
    chunk_id = 1;
    let mut completion_handle = String::new();
    loop {
        let offset = calculate_chunk_offset(chunk_id, CHUNK_SIZE_MULTI);
        if offset >= file_size {
            break;
        }
        let size = calculate_chunk_size(chunk_id, file_size, offset, CHUNK_SIZE_MULTI);
        let chunk_data = &encrypted[offset as usize..(offset + size) as usize];

        let chunk_url = format!("{}/{}", ul_url, offset);

        let mut retries = 0;
        loop {
            let resp = client
                .post(&chunk_url)
                .header("User-Agent", "Mozilla/5.0")
                .body(chunk_data.to_vec())
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let body = r.text().await.unwrap_or_default();
                    // Last chunk returns the completion handle
                    if !body.is_empty() && body.len() < 100 && !body.starts_with('-') {
                        completion_handle = body;
                    }
                    pb.inc(size);
                    break;
                }
                Ok(r) => {
                    retries += 1;
                    if retries > 10 {
                        return Err(anyhow!("Upload chunk {} echoue: HTTP {}", chunk_id, r.status()));
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(retries.min(8))).await;
                }
                Err(e) => {
                    retries += 1;
                    if retries > 10 {
                        return Err(anyhow!("Upload chunk {} echoue: {}", chunk_id, e));
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(retries.min(8))).await;
                }
            }
        }

        chunk_id += 1;
    }

    pb.finish_with_message("done");

    if completion_handle.is_empty() {
        return Err(anyhow!("Pas de completion handle recu"));
    }

    // Build file key for metadata
    let meta_mac = [file_mac[0] ^ file_mac[1], file_mac[2] ^ file_mac[3]];
    let fkey = [
        ul_key[0] ^ ul_key[4],
        ul_key[1] ^ ul_key[5],
        ul_key[2] ^ meta_mac[0],
        ul_key[3] ^ meta_mac[1],
        ul_key[4],
        ul_key[5],
        meta_mac[0],
        meta_mac[1],
    ];

    // Finish upload
    let parent = if remote_parent.is_empty() {
        session.root_id.as_deref().unwrap_or("")
    } else {
        remote_parent
    };

    let result = api::finish_upload(
        client,
        session,
        &filename,
        &ul_key[..4],
        &fkey,
        &completion_handle,
        parent,
    )
    .await?;

    // Extract node handle for link generation
    if let Some(f_arr) = result["f"].as_array() {
        if let Some(first) = f_arr.first() {
            if let Some(h) = first["h"].as_str() {
                println!("  Upload termine! Node: {}", h);
                return Ok(h.to_string());
            }
        }
    }

    println!("  Upload termine!");
    Ok(completion_handle)
}
