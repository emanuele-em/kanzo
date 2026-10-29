mod export;
mod home;
mod mcp;
mod service;
mod store;
mod sync;
mod update;
mod web;

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::home::Home;

/// Self-hosted kanban for humans and agents.
///
/// Run `kanzo` to open the board. Agents connect with `kanzo mcp`.
#[derive(Parser)]
#[command(name = "kanzo", version)]
struct Cli {
    /// Data folder [default: $KANZO_HOME, then ~/kanzo]
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    /// Port for the board [default: from config.toml]
    #[arg(long)]
    port: Option<u16>,

    /// Address to listen on [default: from config.toml]
    #[arg(long, value_name = "ADDR")]
    listen: Option<IpAddr>,

    /// Start the board without opening the browser
    #[arg(long)]
    no_open: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdio (agents launch this, not you)
    Mcp,
    /// Create the data folder; the first run does this too
    Init {
        /// Folder to create [default: the data folder]
        folder: Option<PathBuf>,
    },
    /// Commit the data folder and pull or push it with its git remote
    Sync {
        /// Sync with this git repo from now on, like https://github.com/you/kanzo-data or you/kanzo-data
        #[arg(long, value_name = "URL")]
        repo: Option<String>,
        /// When both sides changed, keep this machine's board and overwrite the remote
        #[arg(long, conflicts_with = "keep_remote")]
        keep_local: bool,
        /// When both sides changed, take the remote board and drop this machine's changes
        #[arg(long)]
        keep_remote: bool,
    },
    /// Write the whole board as Markdown or JSON
    Export {
        /// Output format
        #[arg(long, short, value_enum, default_value = "markdown")]
        format: export::Format,
        /// Only this project
        #[arg(long, short)]
        project: Option<String>,
        /// Write to this file instead of the terminal
        #[arg(long, short, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Install the latest release over this Kanzo, and restart the board's service
    Update,
    /// Start the board automatically at login (macOS and Linux)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start a service that runs the board at login (LaunchAgent on macOS, systemd on Linux)
    Install,
    /// Stop and remove that service
    Uninstall,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // One thread runs all the async work: the board serves one person, and each
    // agent starts its own `kanzo mcp`. Database calls run on a few blocking
    // threads that exit when idle. This keeps kanzo light on small boards.
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(8)
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(run(cli)));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kanzo: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    // Before opening the data folder: it may be the reason to update.
    if let Some(Command::Update) = cli.command {
        return update_kanzo();
    }
    let dir = match &cli.command {
        Some(Command::Init { folder: Some(folder) }) => folder.clone(),
        _ => Home::resolve(cli.home)?,
    };
    let home = Home::open(dir)?;
    match cli.command {
        None => web::serve(home, cli.port, cli.listen, !cli.no_open).await,
        Some(Command::Mcp) => mcp::serve(home).await,
        Some(Command::Init { .. }) => {
            println!("Kanzo data folder ready at {}", home.dir.display());
            Ok(())
        }
        Some(Command::Sync { repo, keep_local, keep_remote }) => {
            if let Some(url) = repo {
                println!("The board syncs with {}.", sync::set_remote(&home, &url, true)?);
            }
            let keep = match (keep_local, keep_remote) {
                (true, _) => Some(sync::Keep::Local),
                (_, true) => Some(sync::Keep::Remote),
                _ => None,
            };
            println!("{}", sync::sync(&home, keep, true)?);
            Ok(())
        }
        Some(Command::Export { format, project, output }) => {
            export::export(&home, format, project.as_deref(), output.as_deref())
        }
        Some(Command::Service { action: ServiceAction::Install }) => service::install(&home),
        Some(Command::Service { action: ServiceAction::Uninstall }) => service::uninstall(),
        Some(Command::Update) => unreachable!("handled above"),
    }
}

/// `kanzo update`: installs the latest release over this program, then restarts the
/// board's service, if there is one, so it runs the new version too.
fn update_kanzo() -> anyhow::Result<()> {
    let Some(latest) = update::latest(true) else {
        anyhow::bail!("can't reach GitHub to check for updates");
    };
    if !update::newer(&latest) {
        println!("Kanzo is up to date ({}).", update::CURRENT);
        return Ok(());
    }
    println!("Installing Kanzo {latest} (you have {})…", update::CURRENT);
    update::install().map_err(anyhow::Error::msg)?;
    if service::restart()? {
        println!("Updated to {latest}, and restarted the board.");
    } else {
        println!("Updated to {latest}.");
    }
    Ok(())
}
