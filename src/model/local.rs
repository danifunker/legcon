//! This computer's files, as the local pane lists them.

use std::path::Path;
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

/// Copy files and folders (dropped from the OS) into `dest`, keeping their names.
pub fn copy_into(
    sources: &[std::path::PathBuf],
    dest: &Path,
    overwrite: bool,
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
    fn copy_one(
        src: &Path,
        dst: &Path,
        overwrite: bool,
        sum: &mut super::transfer::Summary,
        done: &mut u64,
        total: u64,
        ctx: &super::job::JobCtx,
    ) -> anyhow::Result<()> {
        ctx.check_cancel()?;
        let name = dst
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if src.is_dir() {
            std::fs::create_dir_all(dst)?;
            for e in std::fs::read_dir(src)? {
                let e = e?;
                copy_one(
                    &e.path(),
                    &dst.join(e.file_name()),
                    overwrite,
                    sum,
                    done,
                    total,
                    ctx,
                )?;
            }
            return Ok(());
        }
        if dst.exists() && !overwrite {
            sum.skipped.push(name);
            return Ok(());
        }
        ctx.set_phase(format!("Copying {name}"));
        match std::fs::copy(src, dst) {
            Ok(n) => {
                sum.files += 1;
                sum.bytes += n;
                *done += n;
                ctx.set_bytes(*done, total.max(*done));
            }
            Err(e) => sum.failed.push((name, e.to_string())),
        }
        Ok(())
    }
    let total: u64 = sources.iter().map(|p| size_of(p)).sum();
    let mut sum = super::transfer::Summary::default();
    let mut done = 0;
    for src in sources {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dst = dest.join(name);
        // A folder dropped into itself would copy forever.
        if dst.starts_with(src) {
            sum.failed.push((
                name.to_string_lossy().into_owned(),
                "a folder cannot go inside itself".into(),
            ));
            continue;
        }
        copy_one(src, &dst, overwrite, &mut sum, &mut done, total, ctx)?;
    }
    Ok(sum)
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
