//! The `lc` command line: a view over the controllers, for scripts.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::controller::app::default_dir;
use crate::controller::cli_ops;
use crate::controller::machines::{MachinesController, NewMachine, ServiceKind};

#[derive(Parser)]
#[command(name = "lc", version, about = "legcon: connect to 1990s machines")]
struct Cli {
    /// Use this config folder instead of the default (also LEGCON_CONFIG_DIR).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List, add or remove saved machines.
    Machines {
        #[command(subcommand)]
        command: MachinesCommand,
    },
    /// Show or set the home folder every connection starts in.
    Home {
        /// The new home folder; omit to show the current one.
        path: Option<PathBuf>,
    },
    /// List a folder on a machine: `lc ls sparc:/usr/local` (its login folder when no path).
    Ls {
        /// `machine` or `machine:path`.
        target: String,
        /// Use this password instead of the saved one (not stored).
        #[arg(long)]
        password: Option<String>,
    },
    /// Make a login key for a machine, install it over SSH, and check that it works.
    KeyLogin {
        machine: String,
        /// Use this password instead of the saved one (not stored).
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Subcommand)]
enum MachinesCommand {
    List,
    Add {
        /// The machine's name, used as `name:path` elsewhere.
        name: String,
        address: String,
        /// Services it offers: ftp, telnet, smb, nfs, ssh, rexec, demo (repeatable).
        #[arg(long = "service", short = 's', required = true)]
        services: Vec<String>,
        #[arg(long, default_value = "")]
        user: String,
        /// Save this password in machines.json (Base64-encoded, not encrypted).
        #[arg(long)]
        password: Option<String>,
    },
    Remove {
        name: String,
    },
}

/// Run the CLI; the process exit code.
pub fn main() -> i32 {
    let cli = Cli::parse();
    let dir = cli.config_dir.clone().unwrap_or_else(default_dir);
    match run(cli.command, &dir) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("lc: {e}");
            1
        }
    }
}

fn run(command: Command, dir: &std::path::Path) -> Result<(), String> {
    let open = || MachinesController::open(dir).map_err(|e| format!("{e:#}"));
    match command {
        Command::Machines {
            command: MachinesCommand::List,
        } => {
            let rows = open()?.rows(&Default::default());
            if rows.is_empty() {
                println!("No machines yet. Add one with: lc machines add NAME ADDRESS -s ftp");
            }
            for r in rows {
                let pw = if r.password_saved {
                    "password saved"
                } else {
                    "asks for password"
                };
                println!("{:<16} {:<20} {:<20} {}", r.id, r.address, r.services, pw);
            }
            Ok(())
        }
        Command::Machines {
            command:
                MachinesCommand::Add {
                    name,
                    address,
                    services,
                    user,
                    password,
                },
        } => {
            let services = services
                .iter()
                .map(|s| ServiceKind::from_key(s).ok_or_else(|| format!("unknown service {s:?}")))
                .collect::<Result<Vec<_>, _>>()?;
            open()?.add(NewMachine {
                id: name.clone(),
                address,
                services,
                user,
                password,
            })?;
            println!("Added {name}.");
            Ok(())
        }
        Command::Machines {
            command: MachinesCommand::Remove { name },
        } => {
            open()?.remove(&name)?;
            println!("Removed {name}.");
            Ok(())
        }
        Command::Home { path: None } => {
            match cli_ops::home(dir)? {
                Some(h) => println!("{h}"),
                None => println!("No home folder set yet (the window asks on its first start)."),
            }
            Ok(())
        }
        Command::Home { path: Some(p) } => cli_ops::set_home(dir, Some(&p)),
        Command::KeyLogin { machine, password } => {
            let mut machines = open()?;
            let s = cli_ops::set_up_key_login(&mut machines, dir, &machine, password)?;
            println!(
                "Key login works on {machine}: {} key {} ({}), installed in {}.",
                s.algorithm,
                s.key_file.display(),
                if s.new_key { "new" } else { "reused" },
                s.installed_in
            );
            Ok(())
        }
        Command::Ls { target, password } => {
            let (machine, path) = target.split_once(':').unwrap_or((target.as_str(), ""));
            let (shown, rows) = cli_ops::list_remote(&open()?, machine, path, password)?;
            println!("{machine}:{shown}");
            for r in rows {
                let name = if r.is_dir {
                    format!("{}/", r.name)
                } else {
                    r.name
                };
                println!("{:>12}  {name}", r.size);
            }
            Ok(())
        }
    }
}
