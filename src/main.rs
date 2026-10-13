//! `cce-sync`: the laptop side of cce-sync.
//!
//! The server, the device name and the vault come from flags, then the
//! environment, then `~/.config/cce/config.kdl`:
//!
//! ```kdl
//! vault { path "~/Vault" }
//! sync {
//!     server "https://vault.lucas.co"
//!     device "laptop"
//! }
//! ```
//!
//! The token comes from `$CCE_SYNC_TOKEN`, then the keyring (`cce-sync
//! login` puts it there), then `~/.config/cce/cce-sync.token`.

mod daemon;
mod http;
mod import;
mod local;
mod pass;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use http::Client;
use local::Folder;

const USAGE: &str = "\
usage: cce-sync [--server URL] [--device NAME] [--vault DIR] <command>

commands:
  daemon                    keep the vault in step with the server (the service)
  sync                      one pass now (asks the daemon when it runs)
  sync --prefer-local       one pass in which this folder's version of a file
                            replaces the server's where they differ and the
                            folder has no record of either (the first pass
                            after an import the folder has moved on from);
                            the server's version stays in history
  status                    the daemon's state and what the server holds
  history <note>            a file's revisions, newest first
  restore <note> <rev>      put an earlier revision back (as a new one)
  login                     store the token (read from stdin) in the keyring
  import <dir> [--dry-run]  upload a folder, create-only (writes .cce/vault.kdl
                            from .obsidian/)

environment:
  CCE_SYNC_SERVER, CCE_SYNC_TOKEN, CCE_VAULT";

const KEYRING_SERVICE: &str = "cce-sync";

struct Args {
    server: Option<String>,
    device: Option<String>,
    vault: Option<PathBuf>,
    rest: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { server: None, device: None, vault: None, rest: Vec::new() };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => args.server = Some(it.next().ok_or("--server needs a URL")?),
            "--device" => args.device = Some(it.next().ok_or("--device needs a name")?),
            "--vault" => args.vault = Some(it.next().ok_or("--vault needs a folder")?.into()),
            "-h" | "--help" => return Err(String::new()),
            _ => args.rest.push(a),
        }
    }
    Ok(args)
}

/// A value from config.kdl's `sync { <key> "…" }` (or `sync <key>="…"`).
fn config_value(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(cce_vault::config::config_path()).ok()?;
    let doc: kdl::KdlDocument = text.parse().ok()?;
    let node = doc.get("sync")?;
    let child = node.children().and_then(|c| c.get_arg(key)).and_then(|v| v.as_string());
    let prop = node.get(key).and_then(|e| e.value().as_string());
    child.or(prop).map(str::to_string)
}

fn server(args: &Args) -> Result<String, String> {
    args.server
        .clone()
        .or_else(|| std::env::var("CCE_SYNC_SERVER").ok().filter(|s| !s.is_empty()))
        .or_else(|| config_value("server"))
        .ok_or_else(|| {
            "no server: pass --server, set CCE_SYNC_SERVER, or add `sync { server \"…\" }` to config.kdl".to_string()
        })
}

fn device(args: &Args) -> String {
    args.device.clone().or_else(|| config_value("device")).unwrap_or_else(|| "laptop".into())
}

fn token_file() -> PathBuf {
    cce_vault::config::config_path().with_file_name("cce-sync.token")
}

fn token(server: &str) -> Result<String, String> {
    if let Ok(t) = std::env::var("CCE_SYNC_TOKEN") {
        return Ok(t.trim().to_string());
    }
    if let Ok(t) = keyring::Entry::new(KEYRING_SERVICE, server).and_then(|e| e.get_password()) {
        return Ok(t.trim().to_string());
    }
    std::fs::read_to_string(token_file()).map(|t| t.trim().to_string()).map_err(|_| {
        format!("no token for {server}: run `cce-sync login`, set CCE_SYNC_TOKEN, or write {}", token_file().display())
    })
}

fn client(args: &Args) -> Result<Client, String> {
    let server = server(args)?;
    let token = token(&server)?;
    Ok(Client::new(&server, &token, &device(args)))
}

fn vault(args: &Args) -> Result<PathBuf, String> {
    let root = cce_vault::config::vault_root(args.vault.as_deref()).map_err(|e| e.to_string())?;
    root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))
}

/// A note named on the command line: a file inside the vault, or a vault
/// path.
fn vault_path(args: &Args, name: &str) -> Result<String, String> {
    let p = Path::new(name);
    if p.exists() {
        let abs = p.canonicalize().map_err(|e| e.to_string())?;
        let root = vault(args)?;
        if let Ok(rel) = abs.strip_prefix(&root) {
            return rel.to_str().map(str::to_string).ok_or_else(|| "path is not UTF-8".to_string());
        }
    }
    Ok(name.trim_start_matches('/').to_string())
}

fn fmt_time(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(|| ms.to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}

fn print_report(r: &pass::Report) {
    println!(
        "{} down, {} removed, {} up ({} merged), {} deleted, {} pending",
        r.downloaded, r.removed, r.uploaded, r.merged, r.deleted, r.pending
    );
    for (path, copy) in &r.conflicts {
        println!("conflict: {path}: this device's version kept as {copy}");
    }
    for r in &r.refused {
        println!("refused: {r}");
    }
    for s in &r.skipped {
        println!("skipped: {s}");
    }
}

fn status(args: &Args) -> Result<(), String> {
    match daemon::ask("status").and_then(|s| serde_json::from_str::<daemon::DaemonStatus>(&s).ok()) {
        Some(s) => {
            println!("daemon       {} ({}), live socket {}", s.state, s.vault, if s.live { "up" } else { "down" });
            if let Some(t) = s.last_pass {
                println!("last pass    {}", fmt_time(t));
            }
            if let Some(e) = &s.last_error {
                println!("last error   {e}");
            }
            if s.pending > 0 {
                println!("pending      {}", s.pending);
            }
            for (p, c) in &s.conflicts {
                println!("conflict     {p} -> {c}");
            }
            for r in &s.refused {
                println!("refused      {r}");
            }
        }
        None => println!("daemon       not running"),
    }
    let st = client(args)?.status()?;
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    println!("server seq   {}", st.seq);
    println!("files        {}", st.files);
    println!(
        "note text    {:.2} MB of {:.0} MB{}",
        mb(st.text_bytes),
        mb(cce_sync::proto::TEXT_BUDGET_BYTES),
        if st.warn { "  (past the 30 MB warning)" } else { "" }
    );
    println!("attachments  {:.2} MB", mb(st.attachment_bytes));
    println!("database     {:.2} MB", mb(st.database_bytes));
    Ok(())
}

fn login(args: &Args) -> Result<(), String> {
    let server = server(args)?;
    eprintln!("paste the token for {server} and press Enter:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
    let token = line.trim();
    if token.is_empty() {
        return Err("no token given".into());
    }
    Client::new(&server, token, &device(args)).status().map_err(|e| format!("the server did not take it: {e}"))?;
    keyring::Entry::new(KEYRING_SERVICE, &server)
        .and_then(|e| e.set_password(token))
        .map_err(|e| format!("keyring: {e} (write the token to {} instead, mode 600)", token_file().display()))?;
    println!("token stored in the keyring for {server}");
    Ok(())
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    match args.rest.first().map(String::as_str) {
        Some("daemon") => daemon::run(client(&args)?, vault(&args)?),
        Some("sync") => {
            let prefer_local = args.rest.iter().any(|a| a == "--prefer-local");
            if prefer_local && daemon::ask("ping").is_some() {
                return Err("--prefer-local is for before the daemon runs: stop it first \
                            (systemctl --user stop cce-sync)"
                    .into());
            }
            if !prefer_local && daemon::ask("sync").is_some() {
                println!("asked the running daemon to sync");
                return Ok(());
            }
            let http = client(&args)?;
            let root = vault(&args)?;
            let state = daemon::state_path(http.server(), &root);
            let mut saved = daemon::load(&state, http.server(), &root);
            let report = pass::run(&http, &Folder { root }, &mut saved, prefer_local)?;
            daemon::save(&state, &saved)?;
            print_report(&report);
            Ok(())
        }
        Some("status") => status(&args),
        Some("history") => {
            let name = args.rest.get(1).ok_or("history needs a note")?;
            let path = vault_path(&args, name)?;
            let h = client(&args)?.history(&path)?;
            if h.versions.is_empty() {
                return Err(format!("no history for {path}"));
            }
            for v in &h.versions {
                let what = if v.hash.is_some() { format!("{} bytes", v.size) } else { "deleted".into() };
                println!("{:>5}  {}  {:<10} {what}", v.rev, fmt_time(v.at), v.device);
            }
            Ok(())
        }
        Some("restore") => {
            let name = args.rest.get(1).ok_or("restore needs a note and a revision")?;
            let rev: u64 = args.rest.get(2).and_then(|r| r.parse().ok()).ok_or("restore needs a revision number")?;
            let path = vault_path(&args, name)?;
            let new = pass::restore(&client(&args)?, &path, rev)?;
            println!("{path}: revision {rev} restored as revision {new}");
            if daemon::ask("sync").is_none() {
                println!("(no daemon running: `cce-sync sync` brings it to the folder)");
            }
            Ok(())
        }
        Some("login") => login(&args),
        Some("import") => {
            let dir = args.rest.get(1).ok_or("import needs a vault folder")?;
            let dry_run = args.rest.iter().any(|a| a == "--dry-run");
            import::run(&client(&args)?, PathBuf::from(dir).as_path(), dry_run)
        }
        _ => Err(String::new()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is_empty() => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("cce-sync: {e}");
            ExitCode::FAILURE
        }
    }
}
