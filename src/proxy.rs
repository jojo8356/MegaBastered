use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::path::Path;

/// Hard cap so a huge (or hostile) list cannot eat all the memory.
pub const MAX_ENTRIES: usize = 5000;

/// One proxy of a list file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEntry {
    /// URL ready for reqwest, e.g. `socks5h://user:pass@127.0.0.1:9050`.
    pub url: String,
    /// Original line, for display.
    pub raw: String,
}

/// How a proxy is picked at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyMode {
    /// First entry of the list.
    First,
    /// Random entry (useful to spread runs over many exits).
    Random,
}

impl ProxyMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "first" | "1" => Some(ProxyMode::First),
            "random" | "r" | "aleatoire" | "aléatoire" => Some(ProxyMode::Random),
            _ => None,
        }
    }
}

/// Schemes reqwest can handle.
const SCHEMES: [&str; 6] = [
    "http://",
    "https://",
    "socks5h://",
    "socks5://",
    "socks4://",
    "socks4a://",
];

fn has_scheme(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
}

/// Check a proxy URL, returning a readable reason when it cannot be used.
pub fn validate(url: &str) -> Result<()> {
    if !has_scheme(url) {
        return Err(anyhow!(
            "scheme non supporte (attendu http://, https://, socks5://, socks5h://, socks4://)"
        ));
    }

    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let hostport = rest.rsplit('@').next().unwrap_or(rest);
    let hostport = hostport.split('/').next().unwrap_or(hostport);

    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => {
            let port: u16 = port
                .parse()
                .map_err(|_| anyhow!("port invalide ({})", port))?;
            if port == 0 {
                return Err(anyhow!("port invalide (0)"));
            }
            Ok(())
        }
        _ => Err(anyhow!("port manquant (format attendu host:port)")),
    }
}

/// Percent-encode the user and password parts of a proxy URL.
fn encode_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*'
                    | b'+' | b',' | b';' | b'='
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// `host:port` (no scheme, no credentials).
fn is_host_port(value: &str) -> bool {
    match value.rsplit_once(':') {
        Some((host, port)) => {
            !host.trim().is_empty()
                && !port.is_empty()
                && port.len() <= 5
                && port.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn base64_decode_lenient(value: &str) -> Option<Vec<u8>> {
    crate::crypto::url_base64_decode(value).ok()
}

/// Parse one line of a proxy list.
///
/// Accepted shapes (same spirit as MegaBasterd's smart proxy):
///
/// ```text
/// 1.2.3.4:8080                        HTTP proxy (bare host:port)
/// *1.2.3.4:1080                       SOCKS5 proxy (MegaBasterd's `*` marker)
/// http://1.2.3.4:8080                 explicit HTTP
/// socks5://1.2.3.4:1080               SOCKS5, DNS resolved locally
/// socks5h://1.2.3.4:1080              SOCKS5, DNS resolved by the proxy
/// user:pass@1.2.3.4:8080              credentials
/// 1.2.3.4:8080@dXNlcjpwYXNz           credentials, base64(user:pass)
/// # commentaire                       ignored
/// ```
pub fn parse_line(raw: &str) -> Option<ProxyEntry> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
        return None;
    }

    // Drop a trailing " # comment" (keep '#' inside passwords).
    let line = match line.find(" #") {
        Some(index) => line[..index].trim(),
        None => line,
    };
    if line.is_empty() {
        return None;
    }

    // MegaBasterd's legacy SOCKS marker.
    let (socks_marker, line) = match line.strip_prefix('*') {
        Some(rest) => (true, rest.trim()),
        None => (false, line),
    };
    if line.is_empty() {
        return None;
    }

    // Explicit scheme?
    let mut scheme = if socks_marker { "socks5h" } else { "http" };
    let mut body = line;
    for candidate in ["http", "https", "socks5h", "socks5", "socks4a", "socks4"] {
        let prefix = format!("{}://", candidate);
        if body.to_ascii_lowercase().starts_with(&prefix) {
            scheme = candidate;
            body = &body[prefix.len()..];
            break;
        }
    }
    // Keep the proxy side readable / trimmed of any path.
    let body = body.split('/').next().unwrap_or(body).trim();
    if body.is_empty() {
        return None;
    }

    // `host:port@base64(user:pass)` (MegaBasterd) or `user:pass@host:port`.
    let (hostport, credentials) = match body.rsplit_once('@') {
        Some((left, right)) => {
            // Either `host:port@credentials` (MegaBasterd, base64 or plain)
            // or `credentials@host:port` (URL style).
            if is_host_port(left) && !is_host_port(right) {
                (left, Some(right))
            } else if is_host_port(right) && !is_host_port(left) {
                (right, Some(left))
            } else {
                return None;
            }
        }
        None => (body, None),
    };

    if !is_host_port(hostport) {
        return None;
    }
    let (host, port) = hostport.rsplit_once(':')?;
    if host.trim().is_empty() || port.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
        return None;
    }

    let mut url = format!("{}://{}:{}", scheme, host.trim(), port);

    if let Some(creds) = credentials {
        // Either base64("user:pass") or plain "user:pass".
        let decoded = base64_decode_lenient(creds)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .filter(|text| text.contains(':'))
            .unwrap_or_else(|| creds.to_string());

        let (user, pass) = decoded.split_once(':')?;
        if user.is_empty() {
            return None;
        }
        url = format!(
            "{}://{}:{}@{}:{}",
            scheme,
            encode_userinfo(user),
            encode_userinfo(pass),
            host.trim(),
            port
        );
    }

    Some(ProxyEntry {
        url,
        raw: raw.trim().to_string(),
    })
}

/// Parse a whole proxy file (one entry per line).
pub fn parse_list(content: &str) -> Vec<ProxyEntry> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for raw in content.lines() {
        let Some(entry) = parse_line(raw) else {
            continue;
        };
        if !seen.insert(entry.url.clone()) {
            continue;
        }
        entries.push(entry);
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }

    entries
}

/// Load a proxy list from disk.
pub fn load_file(path: &Path) -> Result<Vec<ProxyEntry>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("impossible de lire {}: {}", path.display(), e))?;

    let entries = parse_list(&content);
    if entries.is_empty() {
        return Err(anyhow!(
            "aucun proxy valide dans {} (une entree par ligne, ex: 1.2.3.4:8080)",
            path.display()
        ));
    }

    Ok(entries)
}

/// Pick the starting entry.
pub fn pick_index(mode: ProxyMode, len: usize) -> usize {
    match mode {
        ProxyMode::First => 0,
        ProxyMode::Random => {
            if len <= 1 {
                0
            } else {
                use rand::Rng;
                rand::thread_rng().gen_range(0..len)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url_of(line: &str) -> Option<String> {
        parse_line(line).map(|entry| entry.url)
    }

    #[test]
    fn bare_host_and_port_is_an_http_proxy() {
        assert_eq!(
            url_of("1.2.3.4:8080").as_deref(),
            Some("http://1.2.3.4:8080")
        );
        assert_eq!(
            url_of("proxy.example.com:3128").as_deref(),
            Some("http://proxy.example.com:3128")
        );
    }

    #[test]
    fn schemes_and_socks_marker_are_honoured() {
        assert_eq!(
            url_of("socks5h://127.0.0.1:9050").as_deref(),
            Some("socks5h://127.0.0.1:9050")
        );
        assert_eq!(
            url_of("SOCKS5://127.0.0.1:1080").as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        // MegaBasterd style SOCKS marker.
        assert_eq!(
            url_of("*1.2.3.4:1080").as_deref(),
            Some("socks5h://1.2.3.4:1080")
        );
        assert_eq!(
            url_of("https://1.2.3.4:443").as_deref(),
            Some("https://1.2.3.4:443")
        );
    }

    #[test]
    fn credentials_are_supported() {
        assert_eq!(
            url_of("user:pass@1.2.3.4:8080").as_deref(),
            Some("http://user:pass@1.2.3.4:8080")
        );

        // Password with characters that must be encoded in a URL.
        assert_eq!(
            url_of("me:p@ss word@1.2.3.4:8080").as_deref(),
            Some("http://me:p%40ss%20word@1.2.3.4:8080")
        );

        // MegaBasterd shape: credentials as base64("user:pass") after the host.
        let creds = crate::crypto::url_base64_encode(b"bob:secret");
        let line = format!("1.2.3.4:8080@{}", creds);
        assert_eq!(
            url_of(&line).as_deref(),
            Some("http://bob:secret@1.2.3.4:8080")
        );
    }

    #[test]
    fn comments_blank_lines_and_junk_are_skipped() {
        assert_eq!(url_of(""), None);
        assert_eq!(url_of("   "), None);
        assert_eq!(url_of("# 1.2.3.4:8080"), None);
        assert_eq!(url_of("// 1.2.3.4:8080"), None);
        assert_eq!(url_of("pas un proxy"), None);
        assert_eq!(url_of("1.2.3.4"), None, "port is required");
        assert_eq!(url_of("1.2.3.4:0"), None, "port 0 is invalid");
        assert_eq!(url_of("1.2.3.4:99999"), None, "port out of range");

        // Inline comment.
        assert_eq!(
            url_of("1.2.3.4:8080 # rapide").as_deref(),
            Some("http://1.2.3.4:8080")
        );
    }

    #[test]
    fn list_is_deduplicated_and_capped() {
        let content = "
# mes proxies
1.2.3.4:8080
1.2.3.4:8080
http://1.2.3.4:8080
5.6.7.8:3128
";
        let entries = parse_list(content);
        assert_eq!(entries.len(), 2, "{:?}", entries);
        assert_eq!(entries[0].url, "http://1.2.3.4:8080");
        assert_eq!(entries[1].url, "http://5.6.7.8:3128");
        assert_eq!(entries[0].raw, "1.2.3.4:8080");
    }

    #[test]
    fn every_parsed_url_is_valid() {
        let content = "1.2.3.4:8080\n*9.9.9.9:1080\nsocks5://a.b:1\nhttp://u:p@c.d:2\n";
        for entry in parse_list(content) {
            assert!(
                validate(&entry.url).is_ok(),
                "{} rejected: {:?}",
                entry.url,
                validate(&entry.url)
            );
        }
    }

    #[test]
    fn validation_rejects_unusable_urls() {
        assert!(validate("socks5h://127.0.0.1:9050").is_ok());
        assert!(validate("pas-une-url").is_err());
        assert!(validate("socks5h://127.0.0.1").is_err());
        assert!(validate("socks5h://127.0.0.1:abc").is_err());
    }

    #[test]
    fn selection_modes() {
        assert_eq!(pick_index(ProxyMode::First, 10), 0);
        assert_eq!(pick_index(ProxyMode::Random, 1), 0);
        assert_eq!(pick_index(ProxyMode::Random, 0), 0);
        assert_eq!(ProxyMode::parse("random"), Some(ProxyMode::Random));
        assert_eq!(ProxyMode::parse("FIRST"), Some(ProxyMode::First));
        assert_eq!(ProxyMode::parse("nope"), None);
    }
}
