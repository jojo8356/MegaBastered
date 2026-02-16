use anyhow::{Result, anyhow};
use num_bigint::BigUint;
use rand::Rng;
use reqwest::Client;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::crypto::*;

static SEQNO: AtomicU64 = AtomicU64::new(0);

const API_URL: &str = "https://g.api.mega.co.nz";
const PBKDF2_ITERATIONS: u32 = 100_000;

#[allow(dead_code)]
pub struct MegaSession {
    pub sid: Option<String>,
    pub master_key: Vec<i32>,
    pub rsa_priv_key: Option<[BigUint; 4]>,
    pub password_aes: Vec<i32>,
    pub user_hash: String,
    pub email: String,
    pub root_id: Option<String>,
    pub inbox_id: Option<String>,
    pub trash_id: Option<String>,
}

fn gen_req_id() -> String {
    use rand::Rng;
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..10).map(|_| chars[rng.gen_range(0..chars.len())] as char).collect()
}

fn next_seqno() -> u64 {
    SEQNO.fetch_add(1, Ordering::SeqCst)
}

fn init_seqno() {
    let mut rng = rand::thread_rng();
    let val: u32 = rng.gen();
    SEQNO.store(val as u64, Ordering::SeqCst);
}

pub async fn raw_api_request(client: &Client, request: &str, sid: Option<&str>) -> Result<Value> {
    let seqno = next_seqno();
    let url = if let Some(sid) = sid {
        format!("{}/cs?id={}&sid={}", API_URL, seqno, sid)
    } else {
        format!("{}/cs?id={}", API_URL, seqno)
    };

    let mut retries = 0;
    loop {
        let resp = client
            .post(&url)
            .header("Content-Type", "text/plain;charset=UTF-8")
            .header("User-Agent", "Mozilla/5.0")
            .body(request.to_string())
            .send()
            .await?;

        let status = resp.status();
        if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR && retries < 5 {
            retries += 1;
            tokio::time::sleep(std::time::Duration::from_secs(retries)).await;
            continue;
        }

        let text = resp.text().await?;
        if text.is_empty() && retries < 5 {
            retries += 1;
            tokio::time::sleep(std::time::Duration::from_secs(retries)).await;
            continue;
        }

        let val: Value = serde_json::from_str(&text)?;

        // Check for MEGA error
        if let Some(n) = val.as_i64() {
            if n < 0 {
                return Err(anyhow!("MEGA API error: {}", n));
            }
        }
        if let Some(arr) = val.as_array() {
            if arr.len() == 1 {
                if let Some(n) = arr[0].as_i64() {
                    if n < 0 && n != -1 && n != -3 {
                        return Err(anyhow!("MEGA API error: {}", n));
                    }
                }
            }
        }

        return Ok(val);
    }
}

pub async fn raw_api_request_with_node(
    client: &Client,
    request: &str,
    sid: Option<&str>,
    node_id: &str,
) -> Result<Value> {
    let seqno = next_seqno();
    let mut url = format!("{}/cs?id={}", API_URL, seqno);
    if let Some(sid) = sid {
        url.push_str(&format!("&sid={}", sid));
    }
    url.push_str(&format!("&n={}", node_id));

    let resp = client
        .post(&url)
        .header("Content-Type", "text/plain;charset=UTF-8")
        .header("User-Agent", "Mozilla/5.0")
        .body(request.to_string())
        .send()
        .await?;

    let text = resp.text().await?;
    let val: Value = serde_json::from_str(&text)?;
    Ok(val)
}

// --- Check account version ---

async fn read_account_version(client: &Client, email: &str) -> Result<(i32, Option<String>)> {
    let request = format!(r#"[{{"a":"us0","user":"{}"}}]"#, email);
    let res = raw_api_request(client, &request, None).await?;
    let obj = &res[0];
    let version = obj["v"].as_i64().unwrap_or(1) as i32;
    let salt = obj["s"].as_str().map(|s| s.to_string());
    Ok((version, salt))
}

// --- Check 2FA ---

pub async fn check_2fa(client: &Client, email: &str) -> Result<bool> {
    let request = format!(r#"[{{"a":"mfag","e":"{}"}}]"#, email);
    let res = raw_api_request(client, &request, None).await?;
    Ok(res[0].as_i64().unwrap_or(0) == 1)
}

// --- Login ---

pub async fn login(client: &Client, email: &str, password: &str, pincode: Option<&str>) -> Result<MegaSession> {
    init_seqno();

    let (version, salt) = read_account_version(client, email).await?;

    let (password_aes, user_hash) = if version == 1 {
        // Legacy login
        let _key_bytes = email.to_lowercase().as_bytes().to_vec();
        // Pad to multiple of 4 for i32a
        let key_i32 = bin_to_i32a(&password.as_bytes().to_vec());
        let pkey = mega_prepare_master_key(&key_i32)?;
        let uh = mega_user_hash(email.to_lowercase().as_bytes(), &pkey)?;
        (pkey.to_vec(), uh)
    } else {
        // v2 login with PBKDF2
        let salt_bytes = url_base64_decode(salt.as_ref().ok_or(anyhow!("No salt for v2 account"))?)?;
        let derived = pbkdf2_hmac_sha512(password, &salt_bytes, PBKDF2_ITERATIONS, 32);
        let pass_aes = bin_to_i32a(&derived[..16]);
        let uh = url_base64_encode(&derived[16..32]);
        (pass_aes, uh)
    };

    // Build login request
    let request = if let Some(pin) = pincode {
        format!(
            r#"[{{"a":"us","mfa":"{}","user":"{}","uh":"{}"}}]"#,
            pin, email, user_hash
        )
    } else {
        format!(
            r#"[{{"a":"us","user":"{}","uh":"{}"}}]"#,
            email, user_hash
        )
    };

    let res = raw_api_request(client, &request, None).await?;
    let obj = &res[0];

    // Decrypt master key
    let k = obj["k"].as_str().ok_or(anyhow!("No 'k' in login response"))?;
    let k_bytes = url_base64_decode(k)?;
    let pass_aes_bin = i32a_to_bin(&password_aes[..4]);
    let master_key_bytes = aes_ecb_decrypt(&k_bytes, &pass_aes_bin)?;
    let master_key = bin_to_i32a(&master_key_bytes);

    // Decrypt RSA private key and session ID
    let mut sid = None;
    let mut rsa_priv_key = None;

    if let Some(csid) = obj["csid"].as_str() {
        let privk = obj["privk"].as_str().ok_or(anyhow!("No privk"))?;
        let enc_rsa = url_base64_decode(privk)?;
        let master_key_bin = i32a_to_bin(&master_key);
        let privk_bytes = aes_ecb_decrypt(&enc_rsa, &master_key_bin)?;
        let rsa_keys = extract_rsa_priv_key(&privk_bytes)?;
        rsa_priv_key = Some(rsa_keys.clone());

        // Decrypt session ID
        let csid_bytes = url_base64_decode(csid)?;
        let csid_big = mpi_to_big(&csid_bytes);
        // rsa_keys: [p, q, d, u]
        let raw_sid = rsa_decrypt(&csid_big, &rsa_keys[0], &rsa_keys[1], &rsa_keys[2])?;
        sid = Some(url_base64_encode(&raw_sid[..43.min(raw_sid.len())]));
    }

    let mut session = MegaSession {
        sid,
        master_key,
        rsa_priv_key,
        password_aes: password_aes.to_vec(),
        user_hash,
        email: email.to_string(),
        root_id: None,
        inbox_id: None,
        trash_id: None,
    };

    // Fetch nodes to get root_id
    fetch_nodes(client, &mut session).await?;

    Ok(session)
}

// --- Fetch nodes ---

pub async fn fetch_nodes(client: &Client, session: &mut MegaSession) -> Result<()> {
    let request = r#"[{"a":"f","c":1}]"#;
    let res = raw_api_request(client, request, session.sid.as_deref()).await?;

    if let Some(files) = res[0]["f"].as_array() {
        for node in files {
            let t = node["t"].as_i64().unwrap_or(-1);
            let h = node["h"].as_str().unwrap_or("");
            match t {
                2 => session.root_id = Some(h.to_string()),
                3 => session.inbox_id = Some(h.to_string()),
                4 => session.trash_id = Some(h.to_string()),
                _ => {}
            }
        }
    }
    Ok(())
}

// --- Get download URL for a public link ---

pub async fn get_file_download_url(client: &Client, link: &str, sid: Option<&str>) -> Result<String> {
    let file_id = extract_file_id(link)?;

    let (request, node_id) = if link.contains("#N!") {
        let folder_id = extract_n_folder_id(link)?;
        (
            format!(r#"[{{"a":"g","g":"1","n":"{}"}}]"#, file_id),
            Some(folder_id),
        )
    } else {
        (
            format!(r#"[{{"a":"g","g":"1","p":"{}"}}]"#, file_id),
            None,
        )
    };

    let res = if let Some(nid) = node_id {
        raw_api_request_with_node(client, &request, sid, &nid).await?
    } else {
        raw_api_request(client, &request, sid).await?
    };

    let dl_url = res[0]["g"]
        .as_str()
        .ok_or(anyhow!("No download URL in response"))?;

    Ok(dl_url.to_string())
}

// --- Get file metadata ---

pub async fn get_file_metadata(client: &Client, link: &str, sid: Option<&str>) -> Result<(String, u64, String)> {
    let file_id = extract_file_id(link)?;
    let file_key = extract_file_key(link)?;

    let (request, node_id) = if link.contains("#N!") {
        let folder_id = extract_n_folder_id(link)?;
        (
            format!(r#"[{{"a":"g","n":"{}"}}]"#, file_id),
            Some(folder_id),
        )
    } else {
        (
            format!(r#"[{{"a":"g","p":"{}"}}]"#, file_id),
            None,
        )
    };

    let res = if let Some(nid) = node_id {
        raw_api_request_with_node(client, &request, sid, &nid).await?
    } else {
        raw_api_request(client, &request, sid).await?
    };

    let fsize = res[0]["s"].as_u64().ok_or(anyhow!("No file size"))?;
    let at = res[0]["at"].as_str().ok_or(anyhow!("No attributes"))?;

    let key_bytes = init_mega_link_key(&file_key)?;
    let attrs = decrypt_mega_attr(at, &key_bytes)?;
    let fname = attrs["n"]
        .as_str()
        .ok_or(anyhow!("No filename in attributes"))?
        .to_string();

    Ok((fname, fsize, file_key))
}

// --- Get quota ---

pub async fn get_quota(client: &Client, session: &MegaSession) -> Result<(u64, u64)> {
    let request = r#"[{"a":"uq","xfer":1,"strg":1}]"#;
    let res = raw_api_request(client, request, session.sid.as_deref()).await?;
    let used = res[0]["cstrg"].as_u64().unwrap_or(0);
    let total = res[0]["mstrg"].as_u64().unwrap_or(0);
    Ok((used, total))
}

// --- Init upload ---

pub async fn init_upload(client: &Client, session: &MegaSession, file_size: u64) -> Result<String> {
    let request = format!(r#"[{{"a":"u","s":{}}}]"#, file_size);
    let res = raw_api_request(client, &request, session.sid.as_deref()).await?;
    let ul_url = res[0]["p"].as_str().ok_or(anyhow!("No upload URL"))?.to_string();
    Ok(ul_url)
}

// --- Finish upload ---

pub async fn finish_upload(
    client: &Client,
    session: &MegaSession,
    filename: &str,
    ul_key: &[i32],
    fkey: &[i32],
    completion_handle: &str,
    parent_node: &str,
) -> Result<Value> {
    let ul_key_bin = i32a_to_bin(&ul_key[..4]);
    let enc_att = encrypt_mega_attr(&format!(r#"{{"n":"{}"}}"#, filename), &ul_key_bin)?;
    let master_key_bin = i32a_to_bin(&session.master_key);
    let fkey_bin = i32a_to_bin(fkey);
    let enc_key = aes_ecb_encrypt(&fkey_bin, &master_key_bin)?;

    let req_id = gen_req_id();
    let request = format!(
        r#"[{{"a":"p","t":"{}","n":[{{"h":"{}","t":0,"a":"{}","k":"{}"}}],"i":"{}"}}]"#,
        parent_node,
        completion_handle,
        url_base64_encode(&enc_att),
        url_base64_encode(&enc_key),
        req_id,
    );

    let res = raw_api_request(client, &request, session.sid.as_deref()).await?;
    Ok(res[0].clone())
}

// --- Create directory ---

pub async fn create_dir(
    client: &Client,
    session: &MegaSession,
    name: &str,
    parent_node: &str,
) -> Result<Value> {
    let node_key: Vec<u8> = (0..16).map(|_| rand::thread_rng().gen()).collect();
    let enc_att = encrypt_mega_attr(&format!(r#"{{"n":"{}"}}"#, name), &node_key)?;
    let master_key_bin = i32a_to_bin(&session.master_key);
    let enc_key = aes_ecb_encrypt(&node_key, &master_key_bin)?;
    let req_id = gen_req_id();

    let request = format!(
        r#"[{{"a":"p","t":"{}","n":[{{"h":"xxxxxxxx","t":1,"a":"{}","k":"{}"}}],"i":"{}"}}]"#,
        parent_node,
        url_base64_encode(&enc_att),
        url_base64_encode(&enc_key),
        req_id,
    );

    let res = raw_api_request(client, &request, session.sid.as_deref()).await?;
    Ok(res[0].clone())
}

// --- Export (get public link) ---

#[allow(dead_code)]
pub async fn get_public_file_link(
    client: &Client,
    session: &MegaSession,
    node_handle: &str,
    node_key: &[u8],
) -> Result<String> {
    let request = format!(r#"[{{"a":"l","n":"{}"}}]"#, node_handle);
    let res = raw_api_request(client, &request, session.sid.as_deref()).await?;
    let file_id = res[0].as_str().ok_or(anyhow!("No file id in export response"))?;
    Ok(format!(
        "https://mega.nz/#!{}!{}",
        file_id,
        url_base64_encode(node_key)
    ))
}

// --- Remove node ---

#[allow(dead_code)]
pub async fn remove_node(client: &Client, session: &MegaSession, node_handle: &str) -> Result<()> {
    let request = format!(r#"[{{"a":"d","n":"{}"}}]"#, node_handle);
    raw_api_request(client, &request, session.sid.as_deref()).await?;
    Ok(())
}

// --- List files in account ---

pub async fn list_files(client: &Client, session: &MegaSession) -> Result<Vec<FileNode>> {
    let request = r#"[{"a":"f","c":1}]"#;
    let res = raw_api_request(client, request, session.sid.as_deref()).await?;

    let mut nodes = Vec::new();
    let master_key_bin = i32a_to_bin(&session.master_key);

    if let Some(files) = res[0]["f"].as_array() {
        for node in files {
            let h = node["h"].as_str().unwrap_or("").to_string();
            let p = node["p"].as_str().unwrap_or("").to_string();
            let t = node["t"].as_i64().unwrap_or(-1);
            let s = node["s"].as_u64().unwrap_or(0);

            // Try to decrypt attributes
            let mut name = h.clone();
            if let Some(k_str) = node["k"].as_str() {
                if let Some(key_part) = k_str.split(':').nth(1) {
                    if let Ok(enc_key) = url_base64_decode(key_part) {
                        if let Ok(dec_key) = aes_ecb_decrypt(&enc_key, &master_key_bin) {
                            if let Some(at) = node["a"].as_str() {
                                let file_key = if dec_key.len() >= 32 {
                                    let ik = bin_to_i32a(&dec_key[..32]);
                                    i32a_to_bin(&[
                                        ik[0] ^ ik[4],
                                        ik[1] ^ ik[5],
                                        ik[2] ^ ik[6],
                                        ik[3] ^ ik[7],
                                    ])
                                } else {
                                    dec_key[..16.min(dec_key.len())].to_vec()
                                };
                                if let Ok(attrs) = decrypt_mega_attr(at, &file_key) {
                                    if let Some(n) = attrs["n"].as_str() {
                                        name = n.to_string();
                                    }
                                }
                            }
                        }
                    }
                }
            }

            nodes.push(FileNode {
                handle: h,
                parent: p,
                node_type: t as i32,
                size: s,
                name,
            });
        }
    }

    Ok(nodes)
}

// --- Get folder nodes (public folder link) ---

pub async fn get_folder_nodes(
    client: &Client,
    folder_id: &str,
    folder_key: &str,
) -> Result<Vec<FileNode>> {
    let request = r#"[{"a":"f","c":"1","r":"1","ca":"1"}]"#;
    let res = raw_api_request_with_node(client, request, None, folder_id).await?;

    let folder_key_bytes = url_base64_key_decode(folder_key)?;
    let mut nodes = Vec::new();

    if let Some(files) = res[0]["f"].as_array() {
        for node in files {
            let h = node["h"].as_str().unwrap_or("").to_string();
            let p = node["p"].as_str().unwrap_or("").to_string();
            let t = node["t"].as_i64().unwrap_or(-1);
            let s = node["s"].as_u64().unwrap_or(0);

            let mut name = h.clone();
            #[allow(unused_assignments)]
            let mut node_key_str = String::new();

            if let Some(k_str) = node["k"].as_str() {
                let parts: Vec<&str> = k_str.split(':').collect();
                if parts.len() == 2 && !parts[1].is_empty() {
                    if let Ok(enc_key) = url_base64_decode(parts[1]) {
                        if let Ok(dec_key) = aes_ecb_decrypt(&enc_key, &folder_key_bytes) {
                            node_key_str = url_base64_encode(&dec_key);
                            let file_key = url_base64_key_decode(&node_key_str).unwrap_or(dec_key);
                            if let Some(at) = node["a"].as_str() {
                                if let Ok(attrs) = decrypt_mega_attr(at, &file_key) {
                                    if let Some(n) = attrs["n"].as_str() {
                                        name = n.to_string();
                                    }
                                }
                            }
                        }
                    }
                }
            }

            nodes.push(FileNode {
                handle: h,
                parent: p,
                node_type: t as i32,
                size: s,
                name,
            });
        }
    }

    Ok(nodes)
}

// --- Helper structs ---

#[derive(Debug, Clone)]
pub struct FileNode {
    pub handle: String,
    pub parent: String,
    pub node_type: i32, // 0=file, 1=folder, 2=root, 3=inbox, 4=trash
    pub size: u64,
    pub name: String,
}

// --- Link parsing helpers ---

pub fn normalize_mega_link(link: &str) -> String {
    let mut s = link.to_string();
    // New format -> legacy
    let patterns = [
        (r"mega\.nz/file/([^#]+)#([^\s]+)", "https://mega.nz/#!$1!$2"),
        (r"mega\.nz/folder/([^#]+)#([^\s/]+)/file/([^\s/]+)", "https://mega.nz/#F*$3!$1!$2"),
        (r"mega\.nz/folder/([^#]+)#([^\s]+)", "https://mega.nz/#F!$1!$2"),
    ];
    for (pat, rep) in patterns {
        let re = regex::Regex::new(pat).unwrap();
        s = re.replace(&s, rep).to_string();
    }
    if !s.starts_with("http") && s.contains("mega.nz") {
        s = format!("https://{}", s);
    }
    s
}

pub fn extract_file_id(link: &str) -> Result<String> {
    let link = normalize_mega_link(link);
    // #!ID!KEY or #N!ID!KEY
    let re = regex::Regex::new(r"#.*?!([^!]+)")?;
    let caps = re.captures(&link).ok_or(anyhow!("Cannot extract file ID from link"))?;
    Ok(caps[1].to_string())
}

pub fn extract_file_key(link: &str) -> Result<String> {
    let link = normalize_mega_link(link);
    let re = regex::Regex::new(r"#.*?![^!]+!([^!#\s]+)")?;
    let caps = re.captures(&link).ok_or(anyhow!("Cannot extract file key from link"))?;
    Ok(caps[1].to_string())
}

pub fn extract_n_folder_id(link: &str) -> Result<String> {
    let re = regex::Regex::new(r"###n=(.+)$")?;
    let caps = re.captures(link).ok_or(anyhow!("Cannot extract folder node ID"))?;
    Ok(caps[1].to_string())
}

pub fn extract_folder_id_and_key(link: &str) -> Result<(String, String)> {
    let link = normalize_mega_link(link);
    let re = regex::Regex::new(r"#F!([^!]+)!([^!#\s]+)")?;
    let caps = re.captures(&link).ok_or(anyhow!("Cannot parse folder link"))?;
    Ok((caps[1].to_string(), caps[2].to_string()))
}

pub fn is_folder_link(link: &str) -> bool {
    let n = normalize_mega_link(link);
    n.contains("#F!")
}

pub fn is_mega_link(link: &str) -> bool {
    link.contains("mega.nz") || link.contains("mega.co.nz")
}

pub fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let exp = ((bytes as f64).ln() / 1024f64.ln()).floor() as usize;
    let exp = exp.min(units.len() - 1);
    let val = bytes as f64 / (1u64 << (10 * exp)) as f64;
    format!("{:.2} {}", val, units[exp])
}
