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
    if text.trim().is_empty() {
        return Err(anyhow!("Reponse vide de l'API MEGA"));
    }
    let val: Value = serde_json::from_str(&text)?;
    check_api_error(&val)?;
    Ok(val)
}

/// Turn a `-N` MEGA error payload into a readable error.
fn check_api_error(val: &Value) -> Result<()> {
    let code = match val.as_i64() {
        Some(code) => Some(code),
        None => val
            .as_array()
            .filter(|a| a.len() == 1)
            .and_then(|a| a[0].as_i64()),
    };

    if let Some(code) = code {
        if code < 0 && code != -1 && code != -3 {
            return Err(anyhow!(
                "MEGA API error {} ({})",
                code,
                mega_error_hint(code)
            ));
        }
    }
    Ok(())
}

fn mega_error_hint(code: i64) -> &'static str {
    match code {
        -2 => "argument invalide",
        -4 => "trop de requetes, reessayez dans un instant",
        -8 => "lien expire",
        -9 => "element introuvable ou supprime",
        -11 => "acces refuse",
        -16 => "compte bloque",
        -17 => "quota de transfert depasse",
        -18 => "service temporairement indisponible",
        _ => "erreur inconnue",
    }
}

/// Get a download URL for a node of a public folder.
pub async fn get_node_download_url(
    client: &Client,
    folder_id: &str,
    node_handle: &str,
) -> Result<String> {
    let request = format!(r#"[{{"a":"g","g":"1","n":"{}"}}]"#, node_handle);
    let res = raw_api_request_with_node(client, &request, None, folder_id).await?;

    res[0]["g"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Pas d'URL de telechargement pour le noeud {}", node_handle))
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
        let key_i32 = bin_to_i32a(password.as_bytes());
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

// --- Quota (storage + transfer) ---

/// Everything MEGA reports about quotas (`a:"uq"`).
///
/// MEGA keeps two independent counters:
/// - **storage**: bytes stored in the account (`cstrg` / `mstrg`);
/// - **transfer**: bytes downloaded/streamed, either as an account monthly
///   allowance for paid plans (`mxfer`, `caxfer`, `csxfer`) or as the free
///   *per-IP* rolling window (`tah`: 6 hourly buckets, `bt`: their age).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaInfo {
    /// Storage used / allowed (0 when unknown).
    pub storage_used: u64,
    pub storage_max: u64,
    /// Account transfer allowance (`mxfer`) and what was used (`caxfer`+`tuo`).
    pub transfer_max: u64,
    pub transfer_used: u64,
    /// Bytes served from this account's links (`csxfer` / `tua`).
    pub transfer_served: u64,
    /// Bytes currently reserved by running downloads (`tar`).
    pub transfer_reserved: u64,
    /// Free, IP based quota: six hourly buckets (`tah`).
    pub ip_buckets: Vec<u64>,
    /// Age in seconds of the current bucket window (`bt`).
    pub bucket_age: Option<u64>,
}

impl QuotaInfo {
    /// Bytes downloaded from this IP during the rolling six hour window.
    pub fn ip_transfer_used(&self) -> u64 {
        self.ip_buckets.iter().sum()
    }

    /// True when MEGA reports a paid/account transfer allowance.
    pub fn has_account_transfer(&self) -> bool {
        self.transfer_max > 0 || self.transfer_used > 0
    }

    /// True when the account allowance is spent.
    pub fn account_exhausted(&self) -> bool {
        self.transfer_max > 0 && self.transfer_used >= self.transfer_max
    }
}

/// Read a number that MEGA may send as a plain value or inside an array.
fn json_number(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    value
        .as_array()
        .and_then(|values| values.first())
        .and_then(|first| first.as_u64())
}

fn field(object: &Value, names: &[&str]) -> u64 {
    for name in names {
        if let Some(number) = json_number(&object[*name]) {
            return number;
        }
    }
    0
}

/// Parse an `a:"uq"` response.
fn parse_quota(object: &Value) -> QuotaInfo {
    let ip_buckets = object["tah"]
        .as_array()
        .map(|values| values.iter().filter_map(|value| value.as_u64()).collect())
        .unwrap_or_default();

    // MEGA splits the account transfer usage in an "own" part (`caxfer`, or
    // `tuo` on older responses) and a "served from my links" part (`csxfer` /
    // `tua`); the SDK adds them, so do we.
    let mut info = QuotaInfo {
        storage_used: field(object, &["cstrg"]),
        storage_max: field(object, &["mstrg"]),
        transfer_max: field(object, &["mxfer"]),
        transfer_used: field(object, &["caxfer"]) + field(object, &["tuo"]),
        transfer_served: field(object, &["csxfer"]) + field(object, &["tua"]),
        transfer_reserved: field(object, &["tar"]),
        ip_buckets,
        bucket_age: object["bt"].as_u64(),
    };

    // Anonymous folder links answer per node (`cstrgn`, `mxfern`, ...).
    if info.storage_used == 0 {
        info.storage_used = field(object, &["cstrgn"]);
    }
    if info.storage_max == 0 {
        info.storage_max = field(object, &["mstrgn"]);
    }
    if info.transfer_max == 0 {
        info.transfer_max = field(object, &["mxfern"]);
    }
    if info.transfer_used == 0 {
        info.transfer_used = field(object, &["caxfern"]);
    }

    info
}

/// Ask MEGA for the storage and transfer quotas (`a:"uq"`, `v:2`).
pub async fn get_quota_info(client: &Client, sid: Option<&str>) -> Result<QuotaInfo> {
    let request = r#"[{"a":"uq","xfer":1,"strg":1,"v":2}]"#;
    let res = raw_api_request(client, request, sid).await?;
    let object = res.get(0).cloned().unwrap_or(Value::Null);
    Ok(parse_quota(&object))
}

// --- Get quota ---

#[allow(dead_code)]
pub async fn get_quota(client: &Client, session: &MegaSession) -> Result<(u64, u64)> {
    let info = get_quota_info(client, session.sid.as_deref()).await?;
    Ok((info.storage_used, info.storage_max))
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
    let node_key: Vec<u8> = (0..16).map(|_| rand::thread_rng().gen::<u8>()).collect();
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
            let (name, node_key) = match decrypt_node_key(node, &master_key_bin) {
                Some((key, Some(name))) => (name, key),
                Some((key, None)) => (h.clone(), key),
                None => (h.clone(), String::new()),
            };

            nodes.push(FileNode {
                handle: h,
                parent: p,
                node_type: t as i32,
                size: s,
                name,
                key: node_key,
            });
        }
    }

    Ok(nodes)
}

/// Decrypt a node key coming from a listing entry.
///
/// MEGA sends `<handle>:<base64 key>` and, for imported/shared nodes, several
/// keys separated by `/` (one per sharing chain). Every segment is tried until
/// one of them decrypts the node attributes.
///
/// Returns the full node key (base64 of the 32 raw bytes, as used in file
/// links) and the decrypted name when available.
fn decrypt_node_key(node: &Value, wrapping_key: &[u8]) -> Option<(String, Option<String>)> {
    let k_str = node["k"].as_str()?;
    let attr = node["a"].as_str();
    let mut fallback: Option<String> = None;

    for segment in k_str.split('/') {
        let encoded = segment.rsplit(':').next().unwrap_or(segment);
        if encoded.is_empty() {
            continue;
        }

        let enc_key = match url_base64_decode(encoded) {
            Ok(key) => key,
            Err(_) => continue,
        };
        let dec_key = match aes_ecb_decrypt(&enc_key, wrapping_key) {
            Ok(key) => key,
            Err(_) => continue,
        };
        let key_b64 = url_base64_encode(&dec_key);

        if let Some(attr) = attr {
            if let Ok(attrs) = decrypt_mega_attr(attr, &node_aes_key(&dec_key)) {
                if let Some(name) = attrs["n"].as_str() {
                    return Some((key_b64, Some(name.to_string())));
                }
            }
        }

        if fallback.is_none() {
            fallback = Some(key_b64);
        }
    }

    fallback.map(|key| (key, None))
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

            let (name, node_key) = match decrypt_node_key(node, &folder_key_bytes) {
                Some((key, Some(name))) => (name, key),
                Some((key, None)) => (h.clone(), key),
                None => (h.clone(), String::new()),
            };

            nodes.push(FileNode {
                handle: h,
                parent: p,
                node_type: t as i32,
                size: s,
                name,
                key: node_key,
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
    /// URL-safe base64 of the decrypted node key (32 bytes, or 16 bytes for
    /// legacy nodes). This is the `handle:key` part of a MEGA file link and is
    /// what the download engine needs to decrypt the file.
    pub key: String,
}

impl FileNode {
    pub fn is_file(&self) -> bool {
        self.node_type == 0
    }

    pub fn is_folder(&self) -> bool {
        self.node_type == 1
    }
}

// --- Link parsing helpers ---

/// Characters that are never part of a MEGA link but often come along when the
/// link is copied from a chat, a markdown document or an e-mail.
const LINK_WRAPPERS: &str = "\"'`<>«»“”‘’()[]{}";

/// Extract the actual MEGA URL from whatever the user pasted.
///
/// Handles the usual copy/paste accidents:
/// - markdown `[label](https://mega.nz/...)`
/// - quotes, backticks or angle brackets around the link
/// - trailing punctuation (`.`, `,`, `)`, ...)
/// - leading text ("tiens: https://mega.nz/...")
pub fn clean_link_input(input: &str) -> String {
    let trimmed = input.trim();

    // Markdown link: keep the target, not the label.
    let markdown = regex::Regex::new(r"\[[^\]]*\]\(([^)\s]+)\)").unwrap();
    let text = match markdown.captures(trimmed) {
        Some(caps) => caps[1].to_string(),
        None => trimmed.to_string(),
    };

    // Take the first MEGA URL found in the text.
    let url = regex::Regex::new(
        r#"(?i)(?:https?://)?mega\.(?:nz|co\.nz)/[^\s)\]}>"'«»]+"#,
    )
    .unwrap();
    if let Some(found) = url.find(&text) {
        return found.as_str().to_string();
    }

    // No recognisable URL: just drop wrappers and trailing punctuation.
    text.trim_matches(|c: char| c.is_whitespace() || LINK_WRAPPERS.contains(c))
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_string()
}

pub fn normalize_mega_link(link: &str) -> String {
    let mut s = clean_link_input(link);

    // New format -> legacy. Keys stop at anything that cannot belong to a
    // base64 URL-safe key, so a link pasted with trailing junk stays valid.
    let key = r#"[^\s)\]}>"'«»]+"#;
    let patterns = [
        (
            format!(r"mega\.nz/file/([^#]+)#({})", key),
            "https://mega.nz/#!$1!$2",
        ),
        (
            r#"mega\.nz/folder/([^#]+)#([^\s)\]}>"'«»/]+)/file/([^\s)\]}>"'«»/]+)"#.to_string(),
            "https://mega.nz/#F*$3!$1!$2",
        ),
        (
            format!(r"mega\.nz/folder/([^#]+)#({})", key),
            "https://mega.nz/#F!$1!$2",
        ),
    ];
    for (pat, rep) in patterns {
        let re = regex::Regex::new(&pat).unwrap();
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

/// Extract the shared-folder credentials and, when the URL selects one, the
/// selected sub-folder handle. MEGA's `/folder/<handle>` suffix is a UI
/// selection and must be applied locally after fetching the shared tree.
pub fn extract_folder_link_details(link: &str) -> Result<(String, String, Option<String>)> {
    let cleaned = clean_link_input(link);

    // Modern link with a selected sub-folder:
    // `https://mega.nz/folder/<root>#<key>/folder/<selected>`.
    let modern = regex::Regex::new(
        r"(?i)mega\.(?:nz|co\.nz)/folder/([^#/?]+)#([^/\s?#]+)(?:/folder/([^/\s?#]+))?",
    )?;
    if let Some(caps) = modern.captures(&cleaned) {
        let folder_id = caps[1].to_string();
        let folder_key = caps[2].to_string();
        let selected_folder = caps.get(3).map(|m| m.as_str().to_string());
        validate_folder_key(&folder_key, &cleaned)?;
        return Ok((folder_id, folder_key, selected_folder));
    }

    // Legacy links don't carry a selected-folder suffix.
    let normalized = normalize_mega_link(&cleaned);
    let re = regex::Regex::new(r"#F!([^!]+)!([^!#\s/]+)")?;
    let caps = re
        .captures(&normalized)
        .ok_or_else(|| anyhow!("lien de dossier invalide: {}", cleaned))?;
    let folder_id = caps[1].to_string();
    let folder_key = caps[2].to_string();
    validate_folder_key(&folder_key, &cleaned)?;
    Ok((folder_id, folder_key, None))
}

fn validate_folder_key(folder_key: &str, link: &str) -> Result<()> {
    if url_base64_decode(folder_key).is_err() {
        return Err(anyhow!(
            "cle de dossier illisible ({}). Verifie que le lien est complet et sans texte colle autour.",
            link
        ));
    }
    Ok(())
}

#[cfg(test)]
pub fn extract_folder_id_and_key(link: &str) -> Result<(String, String)> {
    let (folder_id, folder_key, _) = extract_folder_link_details(link)?;
    Ok((folder_id, folder_key))
}

pub fn is_folder_link(link: &str) -> bool {
    let n = normalize_mega_link(link);
    n.contains("#F!")
}

pub fn is_mega_link(link: &str) -> bool {
    let cleaned = clean_link_input(link);
    cleaned.contains("mega.nz") || cleaned.contains("mega.co.nz")
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        decrypt_node_key, extract_folder_id_and_key, extract_folder_link_details, parse_quota,
        QuotaInfo,
    };
    use crate::crypto::{
        aes_ecb_encrypt, encrypt_mega_attr, node_aes_key, url_base64_encode,
    };
    use serde_json::json;

    fn node_key(seed: u8) -> Vec<u8> {
        (0..32u8)
            .map(|i| i.wrapping_mul(11).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn node_key_is_decrypted_with_the_folder_key() {
        let folder_key: Vec<u8> = (0..16u8).collect();
        let key = node_key(5);
        let wrapped = aes_ecb_encrypt(&key, &folder_key).unwrap();
        let attributes =
            encrypt_mega_attr(r#"{"n":"Module 1 - Creer sa boutique"}"#, &node_aes_key(&key))
                .unwrap();

        let entry = json!({
            "h": "HANDLE1",
            "k": format!("HANDLE1:{}", url_base64_encode(&wrapped)),
            "a": url_base64_encode(&attributes),
        });

        let (decoded, name) = decrypt_node_key(&entry, &folder_key).expect("node key");
        assert_eq!(decoded, url_base64_encode(&key));
        assert_eq!(name.as_deref(), Some("Module 1 - Creer sa boutique"));
    }

    #[test]
    fn multi_segment_keys_fall_back_to_the_valid_one() {
        let folder_key: Vec<u8> = (0..16u8).collect();
        let valid = node_key(7);
        let other = node_key(99);

        let wrapped_other = aes_ecb_encrypt(&other, &folder_key).unwrap();
        let wrapped_valid = aes_ecb_encrypt(&valid, &folder_key).unwrap();
        let attributes = encrypt_mega_attr(r#"{"n":"Module 2"}"#, &node_aes_key(&valid)).unwrap();

        let entry = json!({
            "h": "HANDLE2",
            "k": format!(
                "H1:{}/H2:{}",
                url_base64_encode(&wrapped_other),
                url_base64_encode(&wrapped_valid)
            ),
            "a": url_base64_encode(&attributes),
        });

        let (decoded, name) = decrypt_node_key(&entry, &folder_key).expect("node key");
        assert_eq!(decoded, url_base64_encode(&valid));
        assert_eq!(name.as_deref(), Some("Module 2"));
    }

    #[test]
    fn node_without_attributes_keeps_its_key() {
        let folder_key = [3u8; 16];
        let key = node_key(9);
        let wrapped = aes_ecb_encrypt(&key, &folder_key).unwrap();

        let entry = json!({
            "h": "HANDLE3",
            "k": format!("HANDLE3:{}", url_base64_encode(&wrapped)),
        });

        let (decoded, name) = decrypt_node_key(&entry, &folder_key).expect("node key");
        assert_eq!(decoded, url_base64_encode(&key));
        assert!(name.is_none());
    }

    #[test]
    fn node_without_key_is_ignored() {
        let entry = json!({ "h": "HANDLE4", "a": "AAAA" });
        assert!(decrypt_node_key(&entry, &[0u8; 16]).is_none());
    }

    #[test]
    fn folder_links_are_parsed() {
        let (id, key) =
            extract_folder_id_and_key("https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw")
                .unwrap();
        assert_eq!(id, "2wkxkQAB");
        assert_eq!(key, "IbHZz1kP9KvTp9uTJ2iOLw");

        // Sub-folder link: keep the folder key and ignore the selection.
        let (id, key) = extract_folder_id_and_key(
            "https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw/folder/AbCdEfGh",
        )
        .unwrap();
        assert_eq!(id, "2wkxkQAB");
        assert_eq!(key, "IbHZz1kP9KvTp9uTJ2iOLw");

        let (id, key, selected) = extract_folder_link_details(
            "https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw/folder/Ox9G2RoJ",
        )
        .unwrap();
        assert_eq!(id, "2wkxkQAB");
        assert_eq!(key, "IbHZz1kP9KvTp9uTJ2iOLw");
        assert_eq!(selected.as_deref(), Some("Ox9G2RoJ"));

        let (_, _, selected) = extract_folder_link_details(
            "https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw",
        )
        .unwrap();
        assert_eq!(selected, None);
    }

    #[test]
    fn free_ip_quota_is_parsed_from_hourly_buckets() {
        let payload = serde_json::json!({
            "bt": 7200,
            "tah": [1_000_000_000u64, 2_000_000_000u64, 500_000_000u64, -1, -1, -1],
            "tar": 12_345,
            "cstrg": 3_000_000_000u64,
            "mstrg": 21_474_836_480u64,
            "rtt": 0,
            "s": 3600
        });

        let info = parse_quota(&payload);
        // Empty buckets are sent as -1 and skipped.
        assert_eq!(info.ip_buckets.len(), 3);
        assert_eq!(info.ip_transfer_used(), 3_500_000_000);
        assert_eq!(info.bucket_age, Some(7200));
        assert_eq!(info.transfer_reserved, 12_345);
        assert_eq!(info.storage_used, 3_000_000_000);
        assert_eq!(info.storage_max, 21_474_836_480);
        assert!(!info.has_account_transfer());
    }

    #[test]
    fn paid_transfer_allowance_is_parsed() {
        let payload = serde_json::json!({
            "mxfer": 1_099_511_627_776u64,
            "caxfer": 250_000_000_000u64,
            "tuo": 5_000_000_000u64,
            "csxfer": 1_000_000_000u64,
            "tua": 2_000_000_000u64,
            "cstrg": 900_000_000_000u64,
            "mstrg": 2_199_023_255_552u64,
            "tah": [-1, -1, -1, -1, -1, -1]
        });

        let info = parse_quota(&payload);
        assert!(info.has_account_transfer());
        assert_eq!(info.transfer_max, 1_099_511_627_776);
        assert_eq!(info.transfer_used, 255_000_000_000);
        assert_eq!(info.transfer_served, 3_000_000_000);
        assert!(info.ip_buckets.is_empty());
        assert_eq!(info.ip_transfer_used(), 0);
        assert!(!info.account_exhausted());
    }

    #[test]
    fn anonymous_per_node_quota_is_parsed() {
        let payload = serde_json::json!({
            "cstrgn": [7_000_000u64, 3_000_000u64],
            "mstrgn": [10_000_000u64, 10_000_000u64]
        });

        let info = parse_quota(&payload);
        assert_eq!(info.storage_used, 7_000_000);
        assert_eq!(info.storage_max, 10_000_000);
    }

    #[test]
    fn empty_quota_payload_is_harmless() {
        let info = parse_quota(&serde_json::json!({}));
        assert_eq!(info, QuotaInfo::default());
        assert!(!info.has_account_transfer());
        assert!(!info.account_exhausted());
    }

    #[test]
    fn links_survive_a_messy_copy_paste() {
        use super::clean_link_input;

        let expected = "https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw";

        // Markdown link wrapped in quotes (what chat clients produce).
        let pasted = "'[https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw](https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw)'";
        assert_eq!(clean_link_input(pasted), expected);

        // And the folder parsing must work on it.
        let (id, key) = extract_folder_id_and_key(pasted).unwrap();
        assert_eq!(id, "2wkxkQAB");
        assert_eq!(key, "IbHZz1kP9KvTp9uTJ2iOLw");

        // Quotes, angle brackets, trailing period.
        assert_eq!(
            clean_link_input("\"https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw\"."),
            expected
        );
        assert_eq!(
            clean_link_input("<https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw>"),
            expected
        );
        assert_eq!(
            clean_link_input("tiens: https://mega.nz/folder/2wkxkQAB#IbHZz1kP9KvTp9uTJ2iOLw ok ?"),
            expected
        );

        // Already clean.
        assert_eq!(clean_link_input(expected), expected);
    }

    #[test]
    fn truncated_folder_key_is_reported_clearly() {
        // A key with a character that cannot be base64.
        let error = extract_folder_id_and_key("https://mega.nz/folder/2wkxkQAB#IbHZz1k*P9KvTp9u")
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("cle de dossier illisible"),
            "unexpected message: {}",
            error
        );
    }
}
