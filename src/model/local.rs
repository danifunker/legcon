//! This computer's files, as the local pane lists them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// One entry of a local folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since 1970, when the file system gives it.
    pub modified: Option<i64>,
}

/// List `dir`: folders first, then files, each by name ignoring case.
pub fn list_dir(dir: &Path) -> std::io::Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        // An entry that vanished or can't be read is skipped, not fatal for the whole folder.
        let Ok(meta) = item.metadata() else { continue };
        out.push(LocalEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// What a local copy does with one file already in its target folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Existing {
    Overwrite,
    /// Leave it when it is the same file (same size and CRC-32), else overwrite.
    IfDifferent,
    Skip,
    /// Copy under this name in the same folder instead.
    Rename(String),
}

/// The answers for a local copy: one per target file the user named, `rest` for the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choices {
    pub rest: Existing,
    pub each: HashMap<PathBuf, Existing>,
}

impl Choices {
    pub fn all(rest: Existing) -> Choices {
        Choices {
            rest,
            each: HashMap::new(),
        }
    }
}

/// The target files a copy of `sources` into `dest` would land on that are already there,
/// folders walked; each as (source, target).
pub fn clashes(sources: &[PathBuf], dest: &Path) -> Vec<(PathBuf, PathBuf)> {
    fn walk(src: &Path, dst: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
        if src.is_dir() {
            if dst.is_dir() {
                for e in std::fs::read_dir(src).into_iter().flatten().flatten() {
                    walk(&e.path(), &dst.join(e.file_name()), out);
                }
            }
        } else if dst.is_file() {
            out.push((src.to_path_buf(), dst.to_path_buf()));
        }
    }
    let mut out = Vec::new();
    for src in sources {
        if let Some(name) = src.file_name() {
            walk(src, &dest.join(name), &mut out);
        }
    }
    out
}

fn same_file(a: &Path, b: &Path) -> std::io::Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    let crc = |p: &Path| lc_core::checksum::crc32(&mut std::fs::File::open(p)?);
    Ok(crc(a)? == crc(b)?)
}

/// Copy files and folders (dropped from the OS) into `dest`, keeping their names unless the
/// answers give a file another.
pub fn copy_into(
    sources: &[PathBuf],
    dest: &Path,
    choices: &Choices,
    ctx: &super::job::JobCtx,
) -> anyhow::Result<super::transfer::Summary> {
    fn size_of(p: &Path) -> u64 {
        match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => std::fs::read_dir(p)
                .map(|d| d.flatten().map(|e| size_of(&e.path())).sum())
                .unwrap_or(0),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    struct Run<'a> {
        choices: &'a Choices,
        sum: super::transfer::Summary,
        done: u64,
        total: u64,
        ctx: &'a super::job::JobCtx,
    }
    fn copy_one(r: &mut Run, src: &Path, dst: &Path) -> anyhow::Result<()> {
        r.ctx.check_cancel()?;
        let name = dst
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if src.is_dir() {
            std::fs::create_dir_all(dst)?;
            for e in std::fs::read_dir(src)? {
                let e = e?;
                copy_one(r, &e.path(), &dst.join(e.file_name()))?;
            }
            return Ok(());
        }
        let mut target = dst.to_path_buf();
        if dst.exists() {
            match r.choices.each.get(dst).unwrap_or(&r.choices.rest) {
                Existing::Overwrite => {}
                Existing::Skip => {
                    r.sum.skipped.push(name);
                    return Ok(());
                }
                Existing::IfDifferent => {
                    r.ctx.set_phase(format!("Comparing {name}"));
                    if same_file(src, dst).unwrap_or(false) {
                        r.sum.skipped.push(name);
                        return Ok(());
                    }
                }
                Existing::Rename(n) => target = dst.with_file_name(n),
            }
        }
        r.ctx.set_phase(format!("Copying {name}"));
        match std::fs::copy(src, &target) {
            Ok(n) => {
                r.sum.files += 1;
                r.sum.bytes += n;
                r.done += n;
                r.ctx.set_bytes(r.done, r.total.max(r.done));
            }
            Err(e) => r.sum.failed.push((name, e.to_string())),
        }
        Ok(())
    }
    let mut r = Run {
        choices,
        sum: super::transfer::Summary::default(),
        done: 0,
        total: sources.iter().map(|p| size_of(p)).sum(),
        ctx,
    };
    for src in sources {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = dest.join(name);
        // A folder dropped into itself would copy forever.
        if dst.starts_with(src) {
            r.sum.failed.push((
                name.to_string_lossy().into_owned(),
                "a folder cannot go inside itself".into(),
            ));
            continue;
        }
        copy_one(&mut r, src, &dst)?;
    }
    Ok(r.sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_first_then_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"12").unwrap();
        std::fs::write(dir.path().join("A.txt"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        let names: Vec<_> = list_dir(dir.path())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["zdir", "A.txt", "b.txt"]);
    }
}
