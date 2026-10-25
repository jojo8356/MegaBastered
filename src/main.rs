mod api;
mod crypto;
mod download;
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
}

impl AppState {
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
        println!("\n  Lien MEGA invalide!");
        pause();
        return;
    }

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
    if let Ok((used, total)) = api::get_free_quota(&state.client, sid).await {
        println!(
            "  Quota MEGA (cette IP): {} utilises{}",
            api::format_bytes(used),
            if total == 0 {
                String::new()
            } else {
                format!(" sur {}", api::format_bytes(total))
            }
        );
    }
    println!();

    let result = if folder {
        download::download_folder(&state.client, &link, &path, config)
            .await
            .map(|report| {
                if report.quota_exhausted {
                    println!("\n  Dossier interrompu par le quota MEGA (reprise possible)");
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

    match api::get_quota(&state.client, session).await {
        Ok((used, total)) => {
            let pct = if total > 0 {
                (used as f64 / total as f64 * 100.0) as u64
            } else {
                0
            };
            println!("  Utilise: {} / {} ({}%)", api::format_bytes(used), api::format_bytes(total), pct);
            let free = total.saturating_sub(used);
            println!("  Libre:   {}", api::format_bytes(free));
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
    println!();

    let choices = &[
        "Changer dossier de telechargement",
        "Changer connexions paralleles",
        "Changer limite de vitesse",
        "Changer attente quota 509",
        "Retour",
    ];
    let sel = Select::new()
        .with_prompt("Modifier  (q pour revenir)")
        .items(choices)
        .default(4)
        .interact_opt()
        .ok()
        .flatten()
        .unwrap_or(4);

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
}

fn print_cli_help() {
    println!(
        "MegaCLI v{} - client MEGA en Rust\n\n\
Usage:\n  \
  megacli                          menu interactif\n  \
  megacli <lien> [options]         telechargement direct (fichier ou dossier)\n  \
  megacli --list <lien>            affiche l'arborescence sans rien telecharger\n\n\
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
    })
}

fn default_download_dir() -> String {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string())
}

fn build_client(proxy: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder().timeout(std::time::Duration::from_secs(120));

    if let Some(url) = proxy {
        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| anyhow::anyhow!("proxy invalide ({}): {}", url, e))?;
        builder = builder.proxy(proxy);
        println!("Proxy: {}", url);
    }

    Ok(builder.build()?)
}

/// Print what MEGA still allows for this IP address.
async fn print_quota(client: &Client) -> Result<()> {
    match api::get_free_quota(client, None).await {
        Ok((used, total)) => {
            println!(
                "  Quota de transfert pour cette IP: {} utilises sur {}",
                api::format_bytes(used),
                if total == 0 {
                    "inconnu (MEGA ne publie pas la limite)".to_string()
                } else {
                    api::format_bytes(total)
                }
            );
            if total > 0 && used >= total {
                println!("  Quota epuise: attends 1 a 6 h, ou lance avec -q 60 pour attendre.");
            }
        }
        Err(e) => println!("  Quota indisponible: {}", e),
    }
    Ok(())
}

async fn run_cli(args: CliArgs) -> Result<()> {
    if !api::is_mega_link(&args.link) {
        eprintln!("Lien MEGA invalide: {}", args.link);
        std::process::exit(1);
    }

    let client = build_client(args.proxy.as_deref())?;

    if args.quota_only {
        print_quota(&client).await?;
        return Ok(());
    }

    if args.list {
        let entries = download::list_folder(&client, &args.link).await?;
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
        return Ok(());
    }

    let config = download::DownloadConfig {
        workers: args.workers,
        speed_limit: args.speed_limit,
        quota_wait: args.quota_wait,
    };

    print_quota(&client).await?;

    if api::is_folder_link(&args.link) {
        println!("Dossier: {}", args.link);
        println!("Destination: {}", args.dest);
        let report = download::download_folder(&client, &args.link, &args.dest, config).await?;
        if report.quota_exhausted {
            std::process::exit(2);
        }
        if report.failed > 0 {
            std::process::exit(1);
        }
    } else {
        download::download_from_link(&client, &args.link, &args.dest, config).await?;
    }

    Ok(())
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(args) = parse_cli() {
        return run_cli(args).await;
    }

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let mut state = AppState {
        client,
        session: None,
        download_path: default_download_dir(),
        workers: DEFAULT_WORKERS,
        speed_limit: 0,
        quota_wait: std::time::Duration::ZERO,
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
