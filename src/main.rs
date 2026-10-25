mod api;
mod crypto;
mod download;
mod proxy;
mod upload;

use anyhow::Result;
use console::Style;
use dialoguer::{Input, Select};
use reqwest::Client;

use api::MegaSession;

struct AppState {
    client: Client,
    session: Option<MegaSession>,
    download_path: String,
    workers: usize,
    speed_limit: u64,
    /// How long to pause when MEGA answers HTTP 509 (0 = do not wait).
    quota_wait: std::time::Duration,
    /// Optional HTTP/SOCKS5 proxy used for every MEGA request.
    proxy: Option<String>,
    /// Proxies loaded from a list file.
    proxies: Vec<proxy::ProxyEntry>,
    /// Index of the active proxy in `proxies`.
    proxy_index: usize,
}

impl AppState {
    /// Activate the proxy at `index` in the loaded list.
    fn select_proxy(&mut self, index: usize) -> Result<()> {
        if self.proxies.is_empty() {
            return Ok(());
        }
        self.proxy_index = index % self.proxies.len();
        self.proxy = Some(self.proxies[self.proxy_index].url.clone());
        self.apply_proxy()
    }

    fn proxy_label(&self) -> String {
        match &self.proxy {
            Some(url) => url.clone(),
            None => "aucun (connexion directe)".to_string(),
        }
    }

    /// Rebuild the HTTP client after a proxy change.
    fn apply_proxy(&mut self) -> Result<()> {
        self.client = build_client(self.proxy.as_deref())?;
        Ok(())
    }

    fn download_config(&self) -> download::DownloadConfig {
        download::DownloadConfig {
            workers: self.workers,
            speed_limit: self.speed_limit,
            quota_wait: self.quota_wait,
        }
    }
}

fn clear() {
    print!("\x1B[2J\x1B[1;1H");
}

fn pause() {
    let answer: String = Input::new()
        .with_prompt("Appuie sur Entree pour continuer  (q = quitter)")
        .allow_empty(true)
        .default(String::new())
        .interact_text()
        .unwrap_or_default();

    if answer.trim().eq_ignore_ascii_case("q") {
        println!("Bye!");
        std::process::exit(0);
    }
}

fn ask(prompt: &str) -> String {
    Input::new()
        .with_prompt(prompt)
        .interact_text()
        .unwrap_or_default()
}

fn ask_default(prompt: &str, default: &str) -> String {
    Input::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text()
        .unwrap_or_default()
}

// --- Menu handlers ---

async fn menu_download_link(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Telecharger (lien public) --\n"));

    let link = ask("Lien MEGA (ou 'q' pour annuler)");
    if link.is_empty() || link == "q" {
        return;
    }

    if !api::is_mega_link(&link) {
        println!("\n  Lien MEGA invalide: {}", api::clean_link_input(&link));
        println!("  Format attendu: https://mega.nz/folder/<id>#<cle>");
        pause();
        return;
    }
    let link = api::clean_link_input(&link);

    let path = ask_default("Dossier destination", &state.download_path);
    let workers_str = ask_default("Connexions paralleles", &state.workers.to_string());
    let workers: usize = workers_str.parse().unwrap_or(state.workers).clamp(1, 20);
    let folder = api::is_folder_link(&link);

    if folder {
        println!("\n  Dossier public detecte - arborescence complete");
    }
    println!("  Destination: {}", path);
    println!("  Connexions:  {}", workers);
    println!(
        "  Quota 509:   {}",
        if state.quota_wait.is_zero() {
            "arret et reprise plus tard".to_string()
        } else {
            format!("attente de {} min puis reprise", state.quota_wait.as_secs() / 60)
        }
    );
    println!();

    let mut config = state.download_config();
    config.workers = workers;

    let sid = state.session.as_ref().and_then(|s| s.sid.as_deref());
    if let Ok(info) = api::get_quota_info(&state.client, sid).await {
        for line in describe_quota(&info) {
            println!("  {}", line);
        }
    }
    println!();

    let result = if folder {
        download::download_folder(&state.client, &link, &path, config)
            .await
            .map(|report| {
                if report.quota_exhausted {
                    println!("\n  Dossier interrompu par le quota MEGA (reprise possible)");
                    println!("  Relancer plus tard, ou depuis une autre sortie (Parametres -> 6).");
                } else if report.failed == 0 {
                    println!("\n  Dossier telecharge!");
                } else {
                    println!(
                        "\n  Dossier partiellement telecharge ({} en echec)",
                        report.failed
                    );
                }
            })
    } else {
        download::download_from_link(&state.client, &link, &path, config)
            .await
            .map(|_| println!("\n  Telechargement termine!"))
    };

    if let Err(e) = result {
        println!("\n  Erreur: {}", e);
    }

    pause();
}

async fn menu_download_cloud(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Telecharger depuis le cloud --\n"));

    let session = match &state.session {
        Some(s) => s,
        None => {
            println!("  Pas de compte connecte! Va dans 'Gerer les comptes' d'abord.");
            pause();
            return;
        }
    };

    println!("  Chargement des fichiers...\n");
    match api::list_files(&state.client, session).await {
        Ok(nodes) => {
            print_file_tree(&nodes);
            println!();

            let handle = ask("Handle du fichier a telecharger (ou 'q')");
            if handle == "q" || handle.is_empty() {
                return;
            }

            // Find the node
            if let Some(node) = nodes.iter().find(|n| n.handle == handle) {
                println!("\n  Fichier: {} ({})", node.name, api::format_bytes(node.size));
                println!("  [Le telechargement depuis le cloud necessite la cle du noeud]");
                println!("  Utilise plutot le lien public du fichier avec l'option 1.");
            } else {
                println!("  Handle non trouve!");
            }
        }
        Err(e) => println!("  Erreur: {}", e),
    }
    pause();
}

async fn menu_upload(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Uploader un fichier --\n"));

    let session = match &state.session {
        Some(s) => s,
        None => {
            println!("  Pas de compte connecte! Va dans 'Gerer les comptes' d'abord.");
            pause();
            return;
        }
    };

    let local = ask("Chemin local du fichier");
    if local.is_empty() {
        return;
    }

    let remote = ask_default(
        "Dossier destination (handle, vide = racine)",
        session.root_id.as_deref().unwrap_or(""),
    );

    println!();
    match upload::upload_file(&state.client, session, &local, &remote).await {
        Ok(handle) => println!("\n  Upload termine! Handle: {}", handle),
        Err(e) => println!("\n  Erreur: {}", e),
    }

    pause();
}

async fn menu_list_files(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Fichiers cloud --\n"));

    let session = match &state.session {
        Some(s) => s,
        None => {
            println!("  Pas de compte connecte!");
            pause();
            return;
        }
    };

    match api::list_files(&state.client, session).await {
        Ok(nodes) => print_file_tree(&nodes),
        Err(e) => println!("  Erreur: {}", e),
    }
    pause();
}

async fn menu_stream(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Streamer un fichier --\n"));

    let link = ask("Lien MEGA du fichier");
    if link.is_empty() {
        return;
    }

    let players = &["mpv", "vlc", "ffplay"];
    let sel = Select::new()
        .with_prompt("Lecteur  (q pour annuler)")
        .items(players)
        .default(0)
        .interact_opt()
        .ok()
        .flatten();
    let Some(sel) = sel else {
        return;
    };
    let player = players[sel];

    // Get file info
    println!("\n  Recuperation des infos...");
    match api::get_file_metadata(&state.client, &link, None).await {
        Ok((name, size, key)) => {
            println!("  Fichier: {} ({})", name, api::format_bytes(size));

            match api::get_file_download_url(&state.client, &link, None).await {
                Ok(dl_url) => {
                    let aes_key = crypto::init_mega_link_key(&key).unwrap();
                    let aes_iv = crypto::init_mega_link_key_iv(&key).unwrap();

                    println!("  Telechargement et dechiffrement en cours...\n");

                    // Download entire file, decrypt, pipe to player
                    // For large files, a proper streaming approach would be better
                    let resp = state.client.get(&dl_url).send().await;
                    match resp {
                        Ok(r) => {
                            let encrypted = r.bytes().await.unwrap_or_default();
                            let mut decrypted = encrypted.to_vec();
                            let mut cipher = crypto::make_aes_ctr_cipher(&aes_key, &aes_iv);
                            ctr::cipher::StreamCipher::apply_keystream(&mut cipher, &mut decrypted);

                            // Write to temp file and play
                            let tmp_path = std::env::temp_dir().join(format!("megacli_stream_{}", name));
                            tokio::fs::write(&tmp_path, &decrypted).await.ok();

                            println!("  Lancement de {}...", player);
                            let _ = tokio::process::Command::new(player)
                                .arg(tmp_path.to_str().unwrap_or(""))
                                .status()
                                .await;

                            let _ = tokio::fs::remove_file(&tmp_path).await;
                        }
                        Err(e) => println!("  Erreur telechargement: {}", e),
                    }
                }
                Err(e) => println!("  Erreur URL: {}", e),
            }
        }
        Err(e) => println!("  Erreur: {}", e),
    }
    pause();
}

async fn menu_quota(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Espace disque / Quota --\n"));

    let session = match &state.session {
        Some(s) => s,
        None => {
            println!("  Pas de compte connecte!");
            pause();
            return;
        }
    };

    match api::get_quota_info(&state.client, session.sid.as_deref()).await {
        Ok(info) => {
            for line in describe_quota(&info) {
                println!("  {}", line);
            }
            if info.storage_max > 0 {
                println!(
                    "  Stockage libre: {}",
                    api::format_bytes(info.storage_max.saturating_sub(info.storage_used))
                );
            }
        }
        Err(e) => println!("  Erreur: {}", e),
    }
    pause();
}

async fn menu_mkdir(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Creer un dossier --\n"));

    let session = match &state.session {
        Some(s) => s,
        None => {
            println!("  Pas de compte connecte!");
            pause();
            return;
        }
    };

    let name = ask("Nom du dossier");
    if name.is_empty() {
        return;
    }

    let parent = ask_default(
        "Handle du dossier parent (vide = racine)",
        session.root_id.as_deref().unwrap_or(""),
    );

    match api::create_dir(&state.client, session, &name, &parent).await {
        Ok(res) => {
            if let Some(f) = res["f"].as_array() {
                if let Some(first) = f.first() {
                    println!("\n  Dossier cree! Handle: {}", first["h"].as_str().unwrap_or("?"));
                }
            }
        }
        Err(e) => println!("\n  Erreur: {}", e),
    }
    pause();
}

async fn menu_login(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Gerer les comptes --\n"));

    if let Some(ref session) = state.session {
        println!("  Compte actif: {}", session.email);
        println!("  Root ID: {}", session.root_id.as_deref().unwrap_or("?"));
        println!();

        let choices = &["Changer de compte", "Deconnecter", "Retour"];
        let sel = Select::new()
            .with_prompt("Action  (q pour revenir)")
            .items(choices)
            .default(2)
            .interact_opt()
            .ok()
            .flatten()
            .unwrap_or(2);

        match sel {
            0 => {} // Fall through to login
            1 => {
                state.session = None;
                println!("\n  Deconnecte!");
                pause();
                return;
            }
            _ => return,
        }
    }

    let email = ask("Email MEGA");
    if email.is_empty() {
        return;
    }

    let password: String = dialoguer::Password::new()
        .with_prompt("Mot de passe")
        .interact()
        .unwrap_or_default();
    if password.is_empty() {
        return;
    }

    // Check 2FA
    println!("\n  Connexion en cours...");
    let pincode = match api::check_2fa(&state.client, &email).await {
        Ok(true) => {
            let pin = ask("Code 2FA");
            if pin.is_empty() {
                return;
            }
            Some(pin)
        }
        _ => None,
    };

    match api::login(
        &state.client,
        &email,
        &password,
        pincode.as_deref(),
    )
    .await
    {
        Ok(session) => {
            println!("  Connecte en tant que {}!", session.email);
            println!("  Root: {}", session.root_id.as_deref().unwrap_or("?"));
            state.session = Some(session);
        }
        Err(e) => println!("  Erreur de connexion: {}", e),
    }
    pause();
}

async fn menu_settings(state: &mut AppState) {
    clear();
    let title = Style::new().bold().cyan();
    println!("{}", title.apply_to("-- Parametres --\n"));

    println!("  1. Dossier de telechargement: {}", state.download_path);
    println!("  2. Connexions paralleles:     {}", state.workers);
    println!(
        "  3. Limite de vitesse:         {} KiB/s",
        if state.speed_limit == 0 {
            "illimite".to_string()
        } else {
            state.speed_limit.to_string()
        }
    );
    println!(
        "  4. Attente quota 509:         {}",
        if state.quota_wait.is_zero() {
            "desactivee (arret propre)".to_string()
        } else {
            format!("{} min", state.quota_wait.as_secs() / 60)
        }
    );
    println!("  5. Proxy (HTTP/SOCKS5):       {}", state.proxy_label());
    println!(
        "  6. Liste de proxies (.txt):   {}",
        if state.proxies.is_empty() {
            "aucune".to_string()
        } else {
            format!(
                "{} proxy(ies), actif: {}/{}",
                state.proxies.len(),
                state.proxy_index + 1,
                state.proxies.len()
            )
        }
    );
    println!();

    let choices = &[
        "Changer dossier de telechargement",
        "Changer connexions paralleles",
        "Changer limite de vitesse",
        "Changer attente quota 509",
        "Changer le proxy (HTTP/SOCKS5)",
        "Charger une liste de proxies (.txt)",
        "Retour",
    ];
    let sel = Select::new()
        .with_prompt("Modifier  (q pour revenir)")
        .items(choices)
        .default(6)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(6);

    match sel {
        0 => {
            state.download_path = ask_default("Dossier", &state.download_path);
        }
        1 => {
            let w = ask_default("Connexions (1-20)", &state.workers.to_string());
            state.workers = w.parse::<usize>().unwrap_or(6).clamp(1, 20);
        }
        2 => {
            let s = ask_default("Limite KiB/s (0 = illimite)", &state.speed_limit.to_string());
            state.speed_limit = s.parse().unwrap_or(0);
        }
        3 => {
            let current = state.quota_wait.as_secs() / 60;
            let s = ask_default(
                "Attente apres HTTP 509, en minutes (0 = ne pas attendre)",
                &current.to_string(),
            );
            state.quota_wait = std::time::Duration::from_secs(s.parse::<u64>().unwrap_or(0) * 60);
        }
        4 => {
            println!();
            println!("  Exemples:");
            println!("    socks5h://127.0.0.1:9050   (Tor, DNS resolu par le proxy)");
            println!("    socks5://127.0.0.1:1080    (SSH -D, DNS resolu en local)");
            println!("    http://127.0.0.1:8080      (proxy HTTP)");
            println!("  Vide = connexion directe.");
            println!();
            let input = ask_default("Proxy", state.proxy.as_deref().unwrap_or(""));
            let cleaned = input.trim().to_string();
            let previous = state.proxy.clone();
            state.proxy = if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            };

            if let Err(e) = state.apply_proxy() {
                println!("\n  Proxy refuse: {}", e);
                state.proxy = previous;
                let _ = state.apply_proxy();
            } else {
                state.proxies.clear();
                state.proxy_index = 0;
                println!("\n  Proxy actif: {}", state.proxy_label());
            }
        }
        5 => {
            println!();
            println!("  Un proxy par ligne. Formats acceptes:");
            println!("    1.2.3.4:8080            (HTTP)");
            println!("    *1.2.3.4:1080           (SOCKS5)");
            println!("    socks5h://127.0.0.1:9050");
            println!("    user:pass@1.2.3.4:8080");
            println!("  Les lignes vides et celles commencant par # sont ignorees.");
            println!();
            let path = ask_default("Fichier de proxies", "");
            let path = path.trim();
            if path.is_empty() {
                return;
            }

            match proxy::load_file(std::path::Path::new(path)) {
                Ok(entries) => {
                    println!("\n  {} proxy(ies) charge(s)", entries.len());
                    state.proxies = entries;
                    state.proxy_index = 0;
                    if let Err(e) = state.select_proxy(0) {
                        println!("  Proxy refuse: {}", e);
                    } else {
                        println!("  Actif: {}", state.proxy_label());
                    }
                }
                Err(e) => println!("\n  Erreur: {}", e),
            }
        }
        _ => {}
    }
}

// --- Print file tree ---

fn print_file_tree(nodes: &[api::FileNode]) {
    // Find root
    let root = nodes.iter().find(|n| n.node_type == 2);
    let root_handle = root.map(|r| r.handle.as_str()).unwrap_or("");

    fn print_node(nodes: &[api::FileNode], parent: &str, indent: usize) {
        let mut children: Vec<_> = nodes
            .iter()
            .filter(|n| n.parent == parent && n.node_type <= 1)
            .collect();
        // Folders first, then files
        children.sort_by(|a, b| a.node_type.cmp(&b.node_type).then(a.name.cmp(&b.name)));

        for node in children {
            let prefix = "  ".repeat(indent);
            if node.node_type == 1 {
                let folder_style = Style::new().bold().yellow();
                println!("  {}[{}] {}", prefix, node.handle, folder_style.apply_to(&node.name));
                print_node(nodes, &node.handle, indent + 1);
            } else {
                println!(
                    "  {}[{}] {} ({})",
                    prefix,
                    node.handle,
                    node.name,
                    api::format_bytes(node.size)
                );
            }
        }
    }

    print_node(nodes, root_handle, 0);
}

// --- Mode ligne de commande (scriptable) ---

struct CliArgs {
    link: String,
    dest: String,
    workers: usize,
    speed_limit: u64,
    quota_wait: std::time::Duration,
    list: bool,
    /// Print the remaining transfer quota for this IP and exit.
    quota_only: bool,
    /// Optional HTTP/SOCKS5 proxy for every MEGA request (e.g. a VPN endpoint).
    proxy: Option<String>,
    /// Optional file with one proxy per line.
    proxy_file: Option<String>,
    /// Probe every proxy of the list and exit.
    proxy_check: bool,
    /// How the first proxy is picked.
    proxy_mode: proxy::ProxyMode,
}

fn print_cli_help() {
    println!(
        "MegaCLI v{} - client MEGA en Rust\n\n\
Usage:\n  \
  megacli                          menu interactif\n  \
  megacli <lien> [options]         telechargement direct (fichier ou dossier)\n  \
  megacli --list <lien>            affiche l'arborescence sans rien telecharger\n  \
  megacli --proxy-file p.txt --proxy-check   teste une liste de proxies\n\n\
Options:\n  \
  -d, --dest <dir>        dossier de destination (defaut: dossier Telechargements)\n  \
  -w, --workers <1-20>    connexions paralleles par fichier (defaut: 6)\n  \
  -s, --speed <KiB/s>     limite de vitesse, 0 = illimite (defaut: 0)\n  \
  -q, --quota-wait <min>  en cas de quota MEGA (HTTP 509), attendre N minutes\n                          \
puis reprendre au lieu de s'arreter (defaut: 0)\n  \
  -l, --list              lister le contenu du dossier\n  \
      --quota             afficher le quota de transfert restant pour cette IP\n  \
      --proxy <url>       proxy HTTP/SOCKS5 pour toutes les requetes\n  \
                          (ex: socks5h://127.0.0.1:9050)\n  \
      --proxy-file <txt>  liste de proxies, un par ligne\n  \
                          (1.2.3.4:8080, *1.2.3.4:1080, socks5://h:p,\n  \
                           user:pass@h:p, h:p@base64(user:pass))\n  \
      --proxy-check       teste chaque proxy de la liste (latence + quota)\n  \
      --proxy-mode <m>    first | random (defaut: first)\n  \
  -h, --help              cette aide\n\n\
Codes de sortie: 0 succes, 1 erreur, 2 quota MEGA atteint (relancer plus tard).",
        env!("CARGO_PKG_VERSION")
    );
}

/// Returns `None` when no link was given (interactive menu).
fn parse_cli() -> Option<CliArgs> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return None;
    }

    let mut link: Option<String> = None;
    let mut dest: Option<String> = None;
    let mut workers = DEFAULT_WORKERS;
    let mut speed_limit = 0u64;
    let mut quota_wait_min = 0u64;
    let mut list = false;
    let mut proxy: Option<String> = None;
    let mut proxy_file: Option<String> = None;
    let mut proxy_check = false;
    let mut proxy_mode = proxy::ProxyMode::First;
    let mut quota_only = false;

    let mut index = 0usize;
    while index < args.len() {
        let current = args[index].clone();
        match current.as_str() {
            "-h" | "--help" => {
                print_cli_help();
                std::process::exit(0);
            }
            "-l" | "--list" => list = true,
            "--quota" => quota_only = true,
            "--proxy" => {
                index += 1;
                proxy = args.get(index).cloned();
            }
            "--proxy-file" => {
                index += 1;
                proxy_file = args.get(index).cloned();
            }
            "--proxy-check" => proxy_check = true,
            "--proxy-mode" => {
                index += 1;
                match args.get(index).and_then(|value| proxy::ProxyMode::parse(value)) {
                    Some(mode) => proxy_mode = mode,
                    None => {
                        eprintln!("Mode invalide (first | random)");
                        std::process::exit(1);
                    }
                }
            }
            "-d" | "--dest" => {
                index += 1;
                dest = args.get(index).cloned();
            }
            "-w" | "--workers" => {
                index += 1;
                workers = args
                    .get(index)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(DEFAULT_WORKERS)
                    .clamp(1, 20);
            }
            "-s" | "--speed" => {
                index += 1;
                speed_limit = args
                    .get(index)
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
            }
            "-q" | "--quota-wait" => {
                index += 1;
                quota_wait_min = args
                    .get(index)
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
            }
            other if !other.starts_with('-') => link = Some(other.to_string()),
            other => {
                eprintln!("Option inconnue: {} (voir --help)", other);
                std::process::exit(1);
            }
        }

        index += 1;
    }

    let link = match link {
        Some(link) => link,
        None if quota_only || proxy_check => String::new(),
        None => {
            eprintln!("Aucun lien MEGA fourni (voir --help)");
            std::process::exit(1);
        }
    };

    Some(CliArgs {
        link,
        dest: dest.unwrap_or_else(default_download_dir),
        workers,
        speed_limit,
        quota_wait: std::time::Duration::from_secs(quota_wait_min * 60),
        list,
        quota_only,
        proxy,
        proxy_file,
        proxy_check,
        proxy_mode,
    })
}

fn default_download_dir() -> String {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string())
}

/// Proxy configured through MEGACLI_PROXY, if any.
fn env_proxy() -> Option<String> {
    std::env::var("MEGACLI_PROXY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Accept only the proxy URL shapes reqwest can actually use.
fn validate_proxy(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    let supported = ["http://", "https://", "socks5://", "socks5h://", "socks4://"];
    if supported.iter().any(|prefix| lower.starts_with(prefix)) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "scheme non supporte (attendu http://, https://, socks5:// ou socks5h://)"
        ))
    }
}

fn build_client(proxy: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder().timeout(std::time::Duration::from_secs(120));

    if let Some(url) = proxy {
        validate_proxy(url)?;
        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| anyhow::anyhow!("proxy invalide ({}): {}", url, e))?;
        builder = builder.proxy(proxy);
        println!("Proxy: {}", url);
    }

    Ok(builder.build()?)
}

/// Print the quota MEGA reports (storage + transfer).
fn describe_quota(info: &api::QuotaInfo) -> Vec<String> {
    let mut lines = Vec::new();

    if info.has_account_transfer() {
        let percent = if info.transfer_max > 0 {
            format!(
                " ({:.1}%)",
                info.transfer_used as f64 / info.transfer_max as f64 * 100.0
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "Transfert du compte: {} utilises / {}{}",
            api::format_bytes(info.transfer_used),
            if info.transfer_max > 0 {
                api::format_bytes(info.transfer_max)
            } else {
                "limite inconnue".to_string()
            },
            percent
        ));
        if info.transfer_served > 0 {
            lines.push(format!(
                "  dont {} servis depuis tes liens",
                api::format_bytes(info.transfer_served)
            ));
        }
        if info.account_exhausted() {
            lines.push("  Quota du compte epuise: attends le renouvellement mensuel.".to_string());
        }
    }

    if !info.ip_buckets.is_empty() || info.ip_transfer_used() > 0 {
        lines.push(format!(
            "Transfert de cette IP (fenetre glissante de 6 h): {} telecharges sur les 6 dernieres heures",
            api::format_bytes(info.ip_transfer_used())
        ));
        if let Some(age) = info.bucket_age {
            lines.push(format!(
                "  Fenetre horaire en cours depuis {} ({}/6)",
                format_secs(age),
                (age / 3600 + 1).min(6)
            ));
        }
        lines.push(
            "  MEGA ne publie pas la limite gratuite (~5 Go, variable selon pays et charge)."
                .to_string(),
        );
    }

    if info.storage_max > 0 || info.storage_used > 0 {
        lines.push(format!(
            "Stockage: {} / {}",
            api::format_bytes(info.storage_used),
            if info.storage_max > 0 {
                api::format_bytes(info.storage_max)
            } else {
                "illimite/inconnu".to_string()
            }
        ));
    }

    if info.transfer_reserved > 0 {
        lines.push(format!(
            "Reserve par des telechargements en cours: {}",
            api::format_bytes(info.transfer_reserved)
        ));
    }

    lines
}

fn format_secs(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

async fn print_quota(client: &Client, sid: Option<&str>) -> Result<()> {
    match api::get_quota_info(client, sid).await {
        Ok(info) => {
            for line in describe_quota(&info) {
                println!("  {}", line);
            }
        }
        Err(e) => println!("  Quota indisponible: {}", e),
    }
    Ok(())
}

/// Proxies to use, in order. Empty means "direct connection".
fn resolve_proxies(args: &CliArgs) -> Result<Vec<proxy::ProxyEntry>> {
    if let Some(path) = &args.proxy_file {
        return proxy::load_file(std::path::Path::new(path));
    }
    if let Some(url) = &args.proxy {
        proxy::validate(url)?;
        return Ok(vec![proxy::ProxyEntry {
            url: url.clone(),
            raw: url.clone(),
        }]);
    }
    if let Some(path) = std::env::var("MEGACLI_PROXY_FILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return proxy::load_file(std::path::Path::new(&path));
    }
    if let Some(url) = env_proxy() {
        proxy::validate(&url)?;
        return Ok(vec![proxy::ProxyEntry {
            url: url.clone(),
            raw: url,
        }]);
    }
    Ok(Vec::new())
}

/// Shorten a proxy URL for the check table.
fn short_proxy(url: &str) -> String {
    let mut label = url.to_string();
    if label.len() > 42 {
        label.truncate(39);
        label.push_str("...");
    }
    label
}

/// Probe every proxy of the list: can MEGA be reached, how fast, and how much
/// quota is left on that exit IP.
async fn check_proxies(entries: &[proxy::ProxyEntry]) -> Result<()> {
    use futures::stream::{self, StreamExt};

    if entries.is_empty() {
        println!("Aucun proxy a verifier (--proxy-file ou MEGACLI_PROXY_FILE).");
        return Ok(());
    }

    println!(
        "  Verification de {} proxy(ies), 8 en parallele...\n",
        entries.len()
    );

    let mut checks = stream::iter(entries.iter().enumerate().map(|(index, entry)| async move {
        let started = std::time::Instant::now();
        let outcome = match build_client(Some(&entry.url)) {
            Ok(client) => {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    api::get_quota_info(&client, None),
                )
                .await
                {
                    Ok(Ok(info)) => Ok((info, started.elapsed())),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("timeout (20s)".to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        };
        (index, entry, outcome)
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;

    checks.sort_by_key(|(index, _, _)| *index);

    let mut working = 0usize;
    let mut best: Option<(usize, u64)> = None;

    for (index, entry, outcome) in &checks {
        match outcome {
            Ok((info, elapsed)) => {
                working += 1;
                let used = info.ip_transfer_used();
                let better = match best {
                    Some((_, best_used)) => used < best_used,
                    None => true,
                };
                if better {
                    best = Some((*index, used));
                }
                println!(
                    "  {:>3}. {:<42} OK    {:>9} utilises (IP)   {:>5} ms",
                    index + 1,
                    short_proxy(&entry.url),
                    api::format_bytes(used),
                    elapsed.as_millis()
                );
            }
            Err(error) => {
                println!(
                    "  {:>3}. {:<42} ECHEC {}",
                    index + 1,
                    short_proxy(&entry.url),
                    error
                );
            }
        }
    }

    println!(
        "\n  {}/{} proxy(ies) repondent a MEGA.",
        working,
        entries.len()
    );
    if let Some((index, used)) = best {
        println!(
            "  Le moins consomme: {} ({} utilises sur sa fenetre de 6 h) -> numero {}",
            short_proxy(&entries[index].url),
            api::format_bytes(used),
            index + 1
        );
    }

    Ok(())
}

/// True when an error message is about MEGA's transfer quota.
fn is_quota_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("509") || lower.contains("quota")
}

async fn run_cli(args: CliArgs) -> Result<()> {
    let mut args = args;

    // --- Proxies ------------------------------------------------------------
    let proxies = resolve_proxies(&args)?;

    if args.proxy_check {
        return check_proxies(&proxies).await;
    }

    if args.quota_only {
        let client = build_client(proxies.first().map(|entry| entry.url.as_str()))?;
        if let Some(entry) = proxies.first() {
            println!("  Proxy: {}", entry.url);
        }
        return print_quota(&client, None).await;
    }

    if !api::is_mega_link(&args.link) {
        eprintln!("Lien MEGA invalide: {}", api::clean_link_input(&args.link));
        eprintln!("Format attendu: https://mega.nz/folder/<id>#<cle>");
        std::process::exit(1);
    }
    args.link = api::clean_link_input(&args.link);

    let config = download::DownloadConfig {
        workers: args.workers,
        speed_limit: args.speed_limit,
        quota_wait: args.quota_wait,
    };

    // --- Run -----------------------------------------------------------------
    let index = if proxies.is_empty() {
        0
    } else {
        proxy::pick_index(args.proxy_mode, proxies.len())
    };
    let entry = proxies.get(index);
    let client = build_client(entry.map(|entry| entry.url.as_str()))?;

    if let Some(entry) = entry {
        println!("  Proxy {}/{}: {}", index + 1, proxies.len(), entry.url);
    }

    print_quota(&client, None).await?;

    if args.list {
        return print_list(&client, &args).await;
    }

    if api::is_folder_link(&args.link) {
        println!("Dossier: {}", args.link);
        println!("Destination: {}", args.dest);
        let report = download::download_folder(&client, &args.link, &args.dest, config).await?;
        if report.quota_exhausted {
            advise_other_exit(&proxies, index);
            std::process::exit(2);
        }
        if report.failed > 0 {
            std::process::exit(1);
        }
    } else {
        println!("Fichier: {}", args.link);
        match download::download_from_link(&client, &args.link, &args.dest, config).await {
            Ok(()) => {}
            Err(e) if is_quota_message(&e.to_string()) => {
                eprintln!("Quota MEGA atteint: {}", e);
                advise_other_exit(&proxies, index);
                std::process::exit(2);
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// After a 509, explain how to resume from another exit - without ever
/// switching proxy behind the user's back.
fn advise_other_exit(proxies: &[proxy::ProxyEntry], index: usize) {
    if proxies.len() > 1 {
        println!(
            "  Relancer plus tard (fenetre glissante de 6 h), ou depuis une autre sortie:"
        );
        println!(
            "    --proxy \"{}\"   (ou --proxy-mode random pour en tirer une au hasard)",
            proxies[(index + 1) % proxies.len()].url
        );
    } else if proxies.is_empty() {
        println!("  Relancer plus tard (fenetre glissante de 6 h), ou avec un proxy/VPN.");
    }
}

async fn print_list(client: &Client, args: &CliArgs) -> Result<()> {
    let entries = download::list_folder(client, &args.link).await?;
    let base = std::path::Path::new(&args.dest);
    let mut total = 0u64;
    let mut remaining = 0u64;
    let mut files = 0usize;
    let mut done = 0usize;
    let mut partial = 0usize;

    for entry in &entries {
        total += entry.size;
        if entry.is_dir {
            println!("  {}/", entry.path.display());
            continue;
        }

        files += 1;
        let path = base.join(&entry.path);
        let len = std::fs::metadata(&path)
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len());

        match download::classify_disk(len, entry.size) {
            download::DiskState::Complete => {
                done += 1;
                println!(
                    "  [ok]         {}  ({})",
                    entry.path.display(),
                    api::format_bytes(entry.size)
                );
            }
            download::DiskState::Partial(have) => {
                partial += 1;
                remaining += entry.size.saturating_sub(have);
                println!(
                    "  [reprendre]  {}  ({})",
                    entry.path.display(),
                    api::format_bytes(entry.size)
                );
            }
            download::DiskState::Missing => {
                remaining += entry.size;
                println!(
                    "  [manquant]   {}  ({})",
                    entry.path.display(),
                    api::format_bytes(entry.size)
                );
            }
        }
    }

    println!(
        "\n  {} fichier(s), {} au total. Dossier verifie: {}",
        files,
        api::format_bytes(total),
        args.dest
    );
    println!(
        "  {} deja complet(s), {} a reprendre, reste {} a telecharger",
        done,
        partial,
        api::format_bytes(remaining)
    );
    println!(
        "  A ~5 Go par fenetre de 6 h (quota gratuit), prevoir environ {} session(s).",
        remaining.div_ceil(5 * 1024 * 1024 * 1024).max(1)
    );
    Ok(())
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(args) = parse_cli() {
        return run_cli(args).await;
    }

    // Proxy can come from the environment (handy for scripts and the menu).
    let env_list = std::env::var("MEGACLI_PROXY_FILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let proxies = match &env_list {
        Some(path) => match proxy::load_file(std::path::Path::new(path)) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("MEGACLI_PROXY_FILE ignore: {}", e);
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    // A single --proxy / MEGACLI_PROXY wins over the first line of the list.
    let proxy = env_proxy().or_else(|| proxies.first().map(|entry| entry.url.clone()));
    let client = build_client(proxy.as_deref())?;

    let mut state = AppState {
        proxy,
        client,
        session: None,
        download_path: default_download_dir(),
        workers: DEFAULT_WORKERS,
        speed_limit: 0,
        quota_wait: std::time::Duration::ZERO,
        proxies,
        proxy_index: 0,
    };

    let title = Style::new().bold().green();
    let dim = Style::new().dim();

    let menu_items = &[
        "Telecharger (lien MEGA)",
        "Telecharger depuis le cloud",
        "Uploader un fichier",
        "Lister les fichiers cloud",
        "Streamer un fichier",
        "Espace disque / Quota",
        "Creer un dossier",
        "Gerer les comptes",
        "Parametres",
        "Quitter",
    ];

    loop {
        clear();
        println!("{}", title.apply_to(format!("MegaCLI v{}", env!("CARGO_PKG_VERSION"))));
        println!(
            "{}",
            dim.apply_to("Client MEGA en Rust - Download/Upload/Stream")
        );
        if let Some(ref s) = state.session {
            println!("{}", Style::new().green().apply_to(format!("Connecte: {}", s.email)));
        }
        if !state.proxies.is_empty() {
            println!(
                "{}",
                dim.apply_to(format!(
                    "Proxy: {}/{}  {}",
                    state.proxy_index + 1,
                    state.proxies.len(),
                    state.proxy_label()
                ))
            );
        } else if let Some(ref proxy) = state.proxy {
            println!("{}", dim.apply_to(format!("Proxy: {}", proxy)));
        }
        println!();

        let sel = Select::new()
            .with_prompt("Action  (q ou Echap pour quitter)")
            .items(menu_items)
            .default(0)
            .interact_opt();

        match sel {
            Ok(Some(0)) => menu_download_link(&mut state).await,
            Ok(Some(1)) => menu_download_cloud(&mut state).await,
            Ok(Some(2)) => menu_upload(&mut state).await,
            Ok(Some(3)) => menu_list_files(&mut state).await,
            Ok(Some(4)) => menu_stream(&mut state).await,
            Ok(Some(5)) => menu_quota(&mut state).await,
            Ok(Some(6)) => menu_mkdir(&mut state).await,
            Ok(Some(7)) => menu_login(&mut state).await,
            Ok(Some(8)) => menu_settings(&mut state).await,
            Ok(Some(9)) | Ok(None) | Err(_) => {
                println!("Bye!");
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

const DEFAULT_WORKERS: usize = 6;

#[cfg(test)]
mod tests {
    use super::{build_client, describe_quota};
    use crate::api::QuotaInfo;

    #[test]
    fn proxy_urls_are_validated() {
        use super::validate_proxy;

        assert!(validate_proxy("socks5h://127.0.0.1:9050").is_ok());
        assert!(validate_proxy("socks5://127.0.0.1:1080").is_ok());
        assert!(validate_proxy("http://127.0.0.1:8080").is_ok());
        assert!(validate_proxy("SOCKS5H://127.0.0.1:9050").is_ok());

        assert!(validate_proxy("pas-une-url").is_err());
        assert!(validate_proxy("127.0.0.1:9050").is_err());

        assert!(build_client(Some("socks5h://127.0.0.1:9050")).is_ok());
        assert!(build_client(Some("pas-une-url")).is_err());
        assert!(build_client(None).is_ok());
    }

    #[test]
    fn quota_errors_are_recognised() {
        use super::is_quota_message;

        assert!(is_quota_message("HTTP 509 transfer quota exceeded"));
        assert!(is_quota_message("Quota MEGA atteint"));
        assert!(!is_quota_message("connection refused"));
    }

    #[test]
    fn proxy_file_is_loaded_from_disk() {
        use crate::proxy;

        let dir = std::env::temp_dir().join("megacli-proxy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxies.txt");
        std::fs::write(&path, "# test\n1.2.3.4:8080\n*5.6.7.8:1080\n").unwrap();

        let entries = proxy::load_file(&path).unwrap();
        assert_eq!(entries.len(), 2, "{:?}", entries);
        assert_eq!(entries[0].url, "http://1.2.3.4:8080");
        assert_eq!(entries[1].url, "socks5h://5.6.7.8:1080");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn quota_report_mentions_the_ip_window() {
        let info = QuotaInfo {
            ip_buckets: vec![1_000_000_000, 500_000_000],
            bucket_age: Some(1800),
            storage_used: 10,
            storage_max: 20,
            ..QuotaInfo::default()
        };

        let lines = describe_quota(&info);
        let joined = lines.join("\n");
        assert!(joined.contains("fenetre glissante de 6 h"), "{}", joined);
        assert!(joined.contains("Stockage"), "{}", joined);
    }
}
