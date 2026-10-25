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
}

fn clear() {
    print!("\x1B[2J\x1B[1;1H");
}

fn pause() {
    let _: String = Input::new()
        .with_prompt("Appuie sur Entree pour continuer")
        .allow_empty(true)
        .default(String::new())
        .interact_text()
        .unwrap_or_default();
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
    println!();

    let result = if folder {
        download::download_folder(&state.client, &link, &path, workers, state.speed_limit).await
    } else {
        download::download_from_link(&state.client, &link, &path, workers, state.speed_limit).await
    };

    match result {
        Ok(()) => {
            if folder {
                println!("\n  Dossier telecharge!");
            } else {
                println!("\n  Telechargement termine!");
            }
        }
        Err(e) => println!("\n  Erreur: {}", e),
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
        .with_prompt("Lecteur")
        .items(players)
        .default(0)
        .interact()
        .unwrap_or(0);
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
            .with_prompt("Action")
            .items(choices)
            .default(2)
            .interact()
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
    println!();

    let choices = &[
        "Changer dossier de telechargement",
        "Changer connexions paralleles",
        "Changer limite de vitesse",
        "Retour",
    ];
    let sel = Select::new()
        .with_prompt("Modifier")
        .items(choices)
        .default(3)
        .interact()
        .unwrap_or(3);

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

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let download_path = dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    let mut state = AppState {
        client,
        session: None,
        download_path,
        workers: DEFAULT_WORKERS,
        speed_limit: 0,
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
        println!("{}", title.apply_to("MegaCLI v0.1.0"));
        println!(
            "{}",
            dim.apply_to("Client MEGA en Rust - Download/Upload/Stream")
        );
        if let Some(ref s) = state.session {
            println!("{}", Style::new().green().apply_to(format!("Connecte: {}", s.email)));
        }
        println!();

        let sel = Select::new()
            .with_prompt("Action")
            .items(menu_items)
            .default(0)
            .interact();

        match sel {
            Ok(0) => menu_download_link(&mut state).await,
            Ok(1) => menu_download_cloud(&mut state).await,
            Ok(2) => menu_upload(&mut state).await,
            Ok(3) => menu_list_files(&mut state).await,
            Ok(4) => menu_stream(&mut state).await,
            Ok(5) => menu_quota(&mut state).await,
            Ok(6) => menu_mkdir(&mut state).await,
            Ok(7) => menu_login(&mut state).await,
            Ok(8) => menu_settings(&mut state).await,
            Ok(9) | Err(_) => {
                println!("Bye!");
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

const DEFAULT_WORKERS: usize = 6;
