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
    /// What a machine offers (FTP, SMB, SSH, NFS, AFP ...), found without logging in.
    Probe { address: String },
    /// SSH login keys: list, make (old kinds too: DSA, SSH-1), import, export, show, delete.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
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
enum KeysCommand {
    List,
    /// Make a key: rsa2048, rsa1024, dsa1024, ssh1-rsa1024 or ssh1-rsa2048.
    New {
        name: String,
        #[arg(long, default_value = "rsa2048")]
        kind: String,
        #[arg(long, default_value = "legcon")]
        comment: String,
    },
    /// Copy a private key (OpenSSH, PEM, PuTTY .ppk, SSH-1 identity) into legcon's keys.
    Import {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Write the key's files into a folder (private key, .pub, and .secsh.pub for SSH.com).
    Export {
        name: String,
        folder: PathBuf,
    },
    /// Print the public key line and fingerprints.
    Show {
        name: String,
    },
    Delete {
        name: String,
    },
}

#[derive(Subcommand)]
enum MachinesCommand {
    List,
    Add {
        /// The machine's name, used as `name:path` elsewhere.
        name: String,
        address: String,
        /// Services it offers: ftp, telnet, smb, nfs, ssh, rexec, rsh, rlogin, afp, http, gopher,
        /// demo (repeatable). Without any, the machine is probed and what answers is used.
        #[arg(long = "service", short = 's')]
        services: Vec<String>,
        #[arg(long, default_value = "")]
        user: String,
        /// Save this password in machines.json (Base64-encoded, not encrypted).
        #[arg(long)]
        password: Option<String>,
        /// The uid NFS sends (the server trusts it; nobody when not given).
        #[arg(long)]
        uid: Option<u32>,
        /// The gid NFS sends.
        #[arg(long)]
        gid: Option<u32>,
    },
    Remove {
        name: String,
    },
    /// Print a machine's settings, as `key = value` lines that `set` takes.
    Show {
        name: String,
    },
    /// Change settings: `lc machines set sparc charset=MacRoman password=secret os_store=yes`.
    Set {
        name: String,
        /// `key=value` pairs; keys as `lc machines show` prints them.
        #[arg(required = true)]
        settings: Vec<String>,
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
                    uid,
                    gid,
                },
        } => {
            let mut services = services
                .iter()
                .map(|s| ServiceKind::from_key(s).ok_or_else(|| format!("unknown service {s:?}")))
                .collect::<Result<Vec<_>, _>>()?;
            let probed = if services.is_empty() {
                let p = cli_ops::probe(&address)?;
                print!("{}", p.message());
                println!();
                if p.services.is_empty() {
                    return Err("Nothing answered; name its services with -s.".into());
                }
                services = p.services.iter().map(|(k, _)| *k).collect();
                Some(p)
            } else {
                None
            };
            let mut machines = open()?;
            machines.add(NewMachine {
                id: name.clone(),
                address,
                services,
                user,
                password,
            })?;
            if let Some(p) = &probed {
                machines.apply_probe(&name, p)?;
            }
            if uid.is_some() || gid.is_some() {
                machines.set_unix_ids(&name, uid, gid)?;
            }
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
        Command::Machines {
            command: MachinesCommand::Show { name },
        } => {
            print!("{}", show_form(&cli_ops::machine_form(&open()?, &name)?));
            Ok(())
        }
        Command::Machines {
            command: MachinesCommand::Set { name, settings },
        } => {
            cli_ops::set_machine(&mut open()?, &name, &settings)?;
            println!("Saved {name}.");
            Ok(())
        }
        Command::Keys { command } => {
            let mut machines = open()?;
            match command {
                KeysCommand::List => {
                    let rows = cli_ops::key_rows(&machines);
                    if rows.is_empty() {
                        println!("No keys yet. Make one with: lc keys new NAME --kind rsa2048");
                    }
                    for r in rows {
                        println!(
                            "{:<20} {:<16} {}  {}",
                            r.name, r.kind, r.fingerprint, r.used_by
                        );
                    }
                }
                KeysCommand::New {
                    name,
                    kind,
                    comment,
                } => {
                    let label = cli_ops::new_key(&machines, &name, &kind, &comment)?;
                    println!("Made {name} ({label}).");
                }
                KeysCommand::Import { file, name } => {
                    println!(
                        "Imported {}.",
                        cli_ops::import_key(&machines, &file, name.as_deref())?
                    );
                }
                KeysCommand::Export { name, folder } => {
                    for f in cli_ops::export_key(&machines, &name, &folder)? {
                        println!("{}", f.display());
                    }
                }
                KeysCommand::Show { name } => print!("{}", cli_ops::show_key(&machines, &name)?),
                KeysCommand::Delete { name } => {
                    cli_ops::delete_key(&mut machines, &name)?;
                    println!("Deleted {name}.");
                }
            }
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
        Command::Probe { address } => {
            let p = cli_ops::probe(&address)?;
            print!("{}", p.message());
            println!();
            Ok(())
        }
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

/// A form as text: `key = value` lines under section titles; passwords are never printed.
fn show_form(spec: &crate::controller::FormSpec) -> String {
    use crate::controller::FieldKind;
    let mut out = String::new();
    for f in &spec.fields {
        let key = f.id.strip_prefix("machine.").unwrap_or(f.id);
        let value = match &f.kind {
            FieldKind::Heading => {
                out.push_str(&format!(
                    "
[{}]
",
                    f.label
                ));
                continue;
            }
            FieldKind::Text(t) | FieldKind::Folder(t) => t.clone(),
            FieldKind::Secret(t) if t.is_empty() => "(none: asks every time)".into(),
            FieldKind::Secret(_) => "(saved)".into(),
            FieldKind::Check(b) => if *b { "yes" } else { "no" }.into(),
            FieldKind::Choice { options, selected } => format!(
                "{}    one of: {}",
                options.get(*selected).map_or("", String::as_str),
                options.join(", ")
            ),
            FieldKind::Info(t) => format!("{t}    (read-only)"),
        };
        out.push_str(&format!(
            "{key:<16} = {value}
"
        ));
        if !f.hint.is_empty() {
            out.push_str(&format!(
                "{:<16}   {}
",
                "", f.hint
            ));
        }
    }
    out
}
