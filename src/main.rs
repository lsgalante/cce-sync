//! `cce-sync`: the laptop side of cce-sync. Phase 1 has `import` (put a
//! vault folder on the server) and `status`; the daemon proper (watch,
//! push, pull, live socket) is phase 2.
//!
//! The server and token come from `--server` / `$CCE_SYNC_SERVER` and
//! `$CCE_SYNC_TOKEN` or `~/.config/cce/cce-sync.token` (one line, mode
//! 0600). The token moves to the keyring with the daemon.

mod client;
mod import;

use std::path::PathBuf;
use std::process::ExitCode;

use client::Client;

const USAGE: &str = "\
usage: cce-sync [--server URL] [--device NAME] <command>

commands:
  import <vault dir> [--dry-run]   upload a vault folder (skips .obsidian/,
                                   writes .cce/vault.kdl from its settings)
  status                           what the server holds, against the budget

environment:
  CCE_SYNC_SERVER   the server, e.g. https://vault.lucas.co
  CCE_SYNC_TOKEN    the bearer token (else ~/.config/cce/cce-sync.token)";

struct Args {
    server: Option<String>,
    device: String,
    rest: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { server: None, device: "laptop".into(), rest: Vec::new() };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => args.server = Some(it.next().ok_or("--server needs a URL")?),
            "--device" => args.device = it.next().ok_or("--device needs a name")?,
            "-h" | "--help" => return Err(String::new()),
            _ => args.rest.push(a),
        }
    }
    Ok(args)
}

fn token() -> Result<String, String> {
    if let Ok(t) = std::env::var("CCE_SYNC_TOKEN") {
        return Ok(t.trim().to_string());
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("no $HOME")?;
    let path = config.join("cce/cce-sync.token");
    std::fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .map_err(|e| format!("no token: set CCE_SYNC_TOKEN or write {} ({e})", path.display()))
}

fn client(args: &Args) -> Result<Client, String> {
    let server = args
        .server
        .clone()
        .or_else(|| std::env::var("CCE_SYNC_SERVER").ok())
        .ok_or("no server: pass --server or set CCE_SYNC_SERVER")?;
    Ok(Client::new(&server, &token()?, &args.device))
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    match args.rest.first().map(String::as_str) {
        Some("import") => {
            let dir = args.rest.get(1).ok_or("import needs a vault folder")?;
            let dry_run = args.rest.iter().any(|a| a == "--dry-run");
            import::run(&client(&args)?, PathBuf::from(dir).as_path(), dry_run)
        }
        Some("status") => {
            let s = client(&args)?.status()?;
            let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
            println!("seq          {}", s.seq);
            println!("files        {}", s.files);
            println!(
                "note text    {:.2} MB of {:.0} MB{}",
                mb(s.text_bytes),
                mb(cce_sync::proto::TEXT_BUDGET_BYTES),
                if s.warn { "  (past the 30 MB warning)" } else { "" }
            );
            println!("attachments  {:.2} MB", mb(s.attachment_bytes));
            println!("database     {:.2} MB", mb(s.database_bytes));
            Ok(())
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
