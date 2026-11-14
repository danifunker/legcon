//! legcon's own SSH login keys (PLAN.md §5.1, D36): `keys/<name>` (private, PEM) and `<name>.pub`
//! in the config folder. Nothing here writes to `~/.ssh`; import and export go through files the user picks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use lc_ssh::keys::KeyKind;

use super::job::{Job, Notify};

/// One key in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub name: String,
    /// The private half; the public half is next to it as `.pub`.
    pub path: PathBuf,
    /// `ssh-rsa` or `ssh-dss`.
    pub algorithm: String,
    pub bits: usize,
    pub comment: String,
    pub sha256: String,
    pub md5: String,
    /// The SSH-2 public key blob; empty for an SSH-1 key.
    pub blob: Vec<u8>,
    /// The `.pub` line: `ssh-rsa AAAA... comment`, or `bits e n comment` for SSH-1.
    pub public_line: String,
}

impl KeyInfo {
    /// "RSA 2048", "DSA 1024", "SSH-1 RSA 1024".
    pub fn kind_label(&self) -> String {
        let a = match self.algorithm.as_str() {
            "ssh-rsa" => "RSA",
            "ssh-dss" => "DSA",
            "rsa1" => "SSH-1 RSA",
            other => other,
        };
        format!("{a} {}", self.bits)
    }

    /// legcon logs in over SSH-2 only; SSH-1 keys are for other clients and old servers' files.
    pub fn is_ssh2(&self) -> bool {
        self.algorithm != "rsa1"
    }

    /// The `authorized_keys` line.
    pub fn openssh_line(&self) -> String {
        self.public_line.clone()
    }
}

pub fn dir_in(config: &Path) -> PathBuf {
    config.join("keys")
}

/// A key name: letters, digits, `-`, `_` and `.`, as a file name on every system.
pub fn check_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(String::new());
    }
    if n.len() > 40 {
        return Err("At most 40 characters.".into());
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || n.starts_with('.')
        || n.ends_with(".pub")
    {
        return Err("Letters, digits, - _ and . only.".into());
    }
    Ok(())
}

fn read(path: &Path) -> anyhow::Result<KeyInfo> {
    let pub_path = lc_ssh::keys::public_path(path);
    let text = std::fs::read_to_string(&pub_path)
        .with_context(|| format!("reading {}", pub_path.display()))?;
    let line = text.trim();
    let ssh1 = line.starts_with(|c: char| c.is_ascii_digit());
    let (blob, comment, info) = if ssh1 {
        let (e, n, comment) = lc_ssh::keyfile::parse_ssh1_line(line)?;
        (Vec::new(), comment, lc_ssh::keyfile::describe_ssh1(&e, &n))
    } else {
        let (blob, comment) = lc_ssh::keyfile::parse_public_line(line)?;
        let info = lc_ssh::keyfile::describe(&blob)?;
        (blob, comment, info)
    };
    Ok(KeyInfo {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.to_path_buf(),
        algorithm: info.algorithm,
        bits: info.bits,
        comment,
        sha256: info.sha256,
        md5: info.md5,
        blob,
        public_line: line.to_string(),
    })
}

/// Every key in `dir`, by name; files that are not a key pair are left out.
pub fn list(dir: &Path) -> Vec<KeyInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys: Vec<KeyInfo> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_none_or(|x| x != "pub"))
        .filter(|p| lc_ssh::keys::public_path(p).is_file())
        .filter_map(|p| read(&p).ok())
        .collect();
    keys.sort_by_key(|k| k.name.to_lowercase());
    keys
}

/// A free name starting with `base` (`base`, `base-2`, ...).
pub fn free_name(dir: &Path, base: &str) -> String {
    let base: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_start_matches('.');
    let base = if base.is_empty() { "key" } else { base };
    (1..)
        .map(|i| {
            if i == 1 {
                base.to_string()
            } else {
                format!("{base}-{i}")
            }
        })
        .find(|n| !dir.join(n).exists() && !lc_ssh::keys::public_path(&dir.join(n)).exists())
        .unwrap_or_default()
}

fn target(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    check_name(name).map_err(|e| {
        anyhow!(if e.is_empty() {
            "The key needs a name.".into()
        } else {
            e
        })
    })?;
    let path = dir.join(name.trim());
    if path.exists() || lc_ssh::keys::public_path(&path).exists() {
        bail!("There is already a key called {}.", name.trim());
    }
    Ok(path)
}

/// Make a key in the background (RSA 2048 takes a few seconds).
pub fn generate_job(
    dir: &Path,
    name: &str,
    kind: KeyKind,
    comment: &str,
    notify: Arc<dyn Notify>,
) -> anyhow::Result<Job<KeyInfo>> {
    let path = target(dir, name)?;
    let comment = comment.trim().to_string();
    Ok(Job::spawn(
        format!("Make the {} key {}", kind.algorithm(), name.trim()),
        notify,
        move |ctx| {
            ctx.set_phase(format!("Making a {} key", kind.label()));
            if kind.is_ssh1() {
                let (identity, line) = lc_ssh::keys::generate_ssh1(kind, &comment)?;
                lc_ssh::keys::save_ssh1(&identity, &line, &path)?;
            } else {
                let pair = lc_ssh::keys::generate(kind)?;
                lc_ssh::keys::save(&pair, &path, &comment)?;
            }
            read(&path)
        },
    ))
}

/// Copy the private key in `file` into the store as `name`, converted to PEM.
pub fn import(dir: &Path, file: &Path, name: &str) -> anyhow::Result<KeyInfo> {
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    if let Some(line) = lc_ssh::keyfile::ssh1_identity(&data)? {
        let path = target(dir, name)?;
        lc_ssh::keys::save_ssh1(&data, &line, &path)?;
        return read(&path);
    }
    let text = String::from_utf8_lossy(&data);
    let pair = lc_ssh::keyfile::import(&text)?;
    // Keep the comment of a `.pub` next to the file, if there is one.
    let comment = std::fs::read_to_string(lc_ssh::keys::public_path(file))
        .ok()
        .and_then(|t| lc_ssh::keyfile::parse_public_line(t.trim()).ok())
        .map(|(_, c)| c)
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| name.trim().to_string());
    let path = target(dir, name)?;
    lc_ssh::keys::save(&pair, &path, &comment)?;
    read(&path)
}

/// Write the key into `folder`: `<name>` (private), `<name>.pub`, and for SSH-2 keys
/// `<name>.secsh.pub` (SSH.com); the files written.
pub fn export(key: &KeyInfo, folder: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let private = folder.join(&key.name);
    let mut files = vec![private.clone(), lc_ssh::keys::public_path(&private)];
    if key.is_ssh2() {
        files.push(folder.join(format!("{}.secsh.pub", key.name)));
    }
    if let Some(f) = files.iter().find(|f| f.exists()) {
        bail!("{} already exists; choose another folder.", f.display());
    }
    std::fs::copy(&key.path, &files[0])
        .with_context(|| format!("writing {}", files[0].display()))?;
    std::fs::write(&files[1], key.openssh_line() + "\n")?;
    if let Some(f) = files.get(2) {
        std::fs::write(f, lc_ssh::keys::secsh_text(&key.blob, &key.comment))?;
    }
    Ok(files)
}

pub fn delete(key: &KeyInfo) -> anyhow::Result<()> {
    std::fs::remove_file(&key.path).with_context(|| format!("removing {}", key.path.display()))?;
    let _ = std::fs::remove_file(lc_ssh::keys::public_path(&key.path));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_export_list_delete() {
        let store = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let pair = lc_ssh::keys::generate(KeyKind::Dsa1024).unwrap();
        let file = elsewhere.path().join("id_dsa");
        lc_ssh::keys::save(&pair, &file, "dani@indy").unwrap();

        let k = import(store.path(), &file, "indy").unwrap();
        assert_eq!(
            (k.kind_label().as_str(), k.comment.as_str()),
            ("DSA 1024", "dani@indy")
        );
        assert!(import(store.path(), &file, "indy").is_err());
        assert_eq!(free_name(store.path(), "indy"), "indy-2");
        assert_eq!(list(store.path()), vec![k.clone()]);

        let out = tempfile::tempdir().unwrap();
        let files = export(&k, out.path()).unwrap();
        assert_eq!(files.len(), 3);
        assert!(std::fs::read_to_string(&files[2])
            .unwrap()
            .contains("BEGIN SSH2 PUBLIC KEY"));
        assert!(export(&k, out.path()).is_err());

        delete(&k).unwrap();
        assert!(list(store.path()).is_empty());
    }

    #[test]
    fn ssh1_keys_are_kept_and_exported_without_a_secsh_file() {
        let store = tempfile::tempdir().unwrap();
        let (id, line) = lc_ssh::keys::generate_ssh1(KeyKind::Ssh1Rsa1024, "old").unwrap();
        let file = store.path().join("elsewhere");
        std::fs::write(&file, &id).unwrap();
        let k = import(store.path(), &file, "sun").unwrap();
        assert_eq!(k.kind_label(), "SSH-1 RSA 1024");
        assert_eq!(k.public_line, line);
        assert!(!k.is_ssh2());
        let out = tempfile::tempdir().unwrap();
        assert_eq!(export(&k, out.path()).unwrap().len(), 2);
        assert_eq!(std::fs::read(out.path().join("sun")).unwrap(), id);
    }

    #[test]
    fn names_are_plain_file_names() {
        assert!(check_name("vintage-rsa_1.0").is_ok());
        for bad in ["", "a b", "../x", ".hidden", "k.pub", "a/b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }
}
