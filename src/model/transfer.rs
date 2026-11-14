//! Copying between this computer and a machine, folders included, as one job per batch.
//!
//! A batch walks its folders first (listing the remote side, reading the local one), then copies
//! file by file in 64 KB blocks, reporting bytes and the current file, and honours cancel between
//! blocks. Existing targets are overwritten or skipped as the batch says; the caller asked first.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use lc_core::{FileSession, RemotePath, WriteMode};

use super::job::JobCtx;

const BLOCK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

/// One thing the user picked to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub name: String,
    pub is_dir: bool,
}

/// How a batch ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub bytes: u64,
    pub skipped: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// One file to copy, once the folders are walked.
struct Step {
    remote: RemotePath,
    local: PathBuf,
    size: Option<u64>,
    /// A folder to create on the target side, not a file to copy.
    folder: bool,
}

fn walk_remote(
    fs: &mut dyn FileSession,
    remote: RemotePath,
    local: PathBuf,
    out: &mut Vec<Step>,
    ctx: &JobCtx,
) -> anyhow::Result<()> {
    ctx.check_cancel()?;
    out.push(Step {
        remote: remote.clone(),
        local: local.clone(),
        size: None,
        folder: true,
    });
    for e in fs.list(&remote)? {
        let (r, l) = (remote.join(&e.name), local.join(&e.name));
        if e.is_folder() {
            walk_remote(fs, r, l, out, ctx)?;
        } else if e.kind == lc_core::EntryKind::File || e.kind == lc_core::EntryKind::Other {
            out.push(Step {
                remote: r,
                local: l,
                size: e.size,
                folder: false,
            });
        }
    }
    Ok(())
}

fn walk_local(
    local: PathBuf,
    remote: RemotePath,
    out: &mut Vec<Step>,
    ctx: &JobCtx,
) -> anyhow::Result<()> {
    ctx.check_cancel()?;
    out.push(Step {
        remote: remote.clone(),
        local: local.clone(),
        size: None,
        folder: true,
    });
    for item in std::fs::read_dir(&local).with_context(|| format!("reading {}", local.display()))? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        let meta = item.metadata()?;
        if meta.is_dir() {
            walk_local(item.path(), remote.join(&name), out, ctx)?;
        } else {
            out.push(Step {
                remote: remote.join(&name),
                local: item.path(),
                size: Some(meta.len()),
                folder: false,
            });
        }
    }
    Ok(())
}

fn pump(
    from: &mut dyn Read,
    to: &mut dyn Write,
    done: &mut u64,
    total: u64,
    ctx: &JobCtx,
) -> anyhow::Result<u64> {
    let mut buf = vec![0u8; BLOCK];
    let mut n_all = 0;
    loop {
        ctx.check_cancel()?;
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n])?;
        n_all += n as u64;
        *done += n as u64;
        if total > 0 {
            ctx.set_bytes(*done, total.max(*done));
        } else {
            ctx.set_bytes_so_far(*done);
        }
    }
    to.flush()?;
    Ok(n_all)
}

/// What one copy batch is: its direction, the two folders, and the picks with their sizes.
pub struct Batch<'a> {
    pub dir: Direction,
    pub remote_dir: &'a RemotePath,
    pub local_dir: &'a Path,
    pub picks: &'a [Pick],
    pub sizes: &'a [Option<u64>],
    /// False skips targets that already exist.
    pub overwrite: bool,
}

/// Delete a remote file, or a folder with everything in it.
pub fn remove_tree(
    fs: &mut dyn FileSession,
    path: &RemotePath,
    is_dir: bool,
) -> lc_core::Result<()> {
    if is_dir {
        for e in fs.list(path)? {
            remove_tree(fs, &path.join(&e.name), e.is_folder())?;
        }
    }
    fs.remove(path)
}

/// Copy a batch between this computer and the machine.
pub fn copy(fs: &mut dyn FileSession, b: &Batch, ctx: &JobCtx) -> anyhow::Result<Summary> {
    let (dir, remote_dir, local_dir, picks, sizes, overwrite) = (
        b.dir,
        b.remote_dir,
        b.local_dir,
        b.picks,
        b.sizes,
        b.overwrite,
    );
    ctx.set_phase("Looking at what to copy");
    let mut steps = Vec::new();
    for (p, size) in picks.iter().zip(sizes) {
        let (r, l) = (remote_dir.join(&p.name), local_dir.join(&p.name));
        match (dir, p.is_dir) {
            (Direction::Download, true) => walk_remote(fs, r, l, &mut steps, ctx)?,
            (Direction::Upload, true) => walk_local(l, r, &mut steps, ctx)?,
            _ => steps.push(Step {
                remote: r,
                local: l,
                size: *size,
                folder: false,
            }),
        }
    }
    let total: u64 = steps.iter().filter_map(|s| s.size).sum();
    let files = steps.iter().filter(|s| !s.folder).count();
    let existing: Vec<String> = match dir {
        Direction::Upload => {
            let mut names = Vec::new();
            if let Ok(list) = fs.list(remote_dir) {
                names = list.into_iter().map(|e| e.name).collect();
            }
            names
        }
        Direction::Download => Vec::new(),
    };
    let mut sum = Summary::default();
    let mut done = 0u64;
    let mut n = 0;
    for s in &steps {
        ctx.check_cancel()?;
        if s.folder {
            match dir {
                Direction::Download => std::fs::create_dir_all(&s.local)?,
                // An existing folder is fine; anything else shows up when its files fail.
                Direction::Upload => {
                    let _ = fs.mkdir(&s.remote);
                }
            }
            continue;
        }
        n += 1;
        let name = s.remote.name().to_string();
        ctx.set_phase(format!("{n} of {files}: {name}"));
        let exists = match dir {
            Direction::Download => s.local.exists(),
            Direction::Upload => {
                s.remote.parent().as_ref() == Some(remote_dir) && existing.contains(&name)
            }
        };
        if exists && !overwrite {
            sum.skipped.push(name);
            done += s.size.unwrap_or(0);
            continue;
        }
        let r: anyhow::Result<u64> = (|| match dir {
            Direction::Download => {
                let mut src = fs.open_read(&s.remote, 0)?;
                let mut dst = File::create(&s.local)
                    .with_context(|| format!("creating {}", s.local.display()))?;
                pump(&mut src, &mut dst, &mut done, total, ctx)
            }
            Direction::Upload => {
                let mut src = File::open(&s.local)
                    .with_context(|| format!("opening {}", s.local.display()))?;
                let size = src.metadata()?.len();
                let mut dst = fs.open_write_sized(&s.remote, WriteMode::Overwrite, size)?;
                pump(&mut src, &mut dst, &mut done, total, ctx)
            }
        })();
        match r {
            Ok(bytes) => {
                sum.files += 1;
                sum.bytes += bytes;
            }
            Err(e) if e.downcast_ref::<super::job::Cancelled>().is_some() => return Err(e),
            Err(e) => sum.failed.push((name, format!("{e:#}"))),
        }
    }
    if files > 0 && sum.files == 0 && !sum.failed.is_empty() && sum.skipped.is_empty() {
        bail!("nothing was copied: {}", sum.failed[0].1);
    }
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::job::{Job, NoNotify};
    use lc_core::memory::MemoryFs;
    use lc_core::PathStyle;
    use std::sync::{Arc, Mutex};

    fn run(
        fs: MemoryFs,
        dir: Direction,
        local: PathBuf,
        picks: Vec<Pick>,
        overwrite: bool,
    ) -> Summary {
        let fs = Arc::new(Mutex::new(fs));
        let job = Job::spawn("copy", Arc::new(NoNotify), move |ctx| {
            let mut fs = fs.lock().unwrap();
            let sizes = vec![None; picks.len()];
            let home = RemotePath::parse(PathStyle::Unix, "/home");
            let b = Batch {
                dir,
                remote_dir: &home,
                local_dir: &local,
                picks: &picks,
                sizes: &sizes,
                overwrite,
            };
            copy(&mut *fs, &b, ctx)
        });
        job.wait();
        job.take_result().expect("the copy finished")
    }

    #[test]
    fn download_a_folder_then_upload_with_skip() {
        let fs = MemoryFs::new(PathStyle::Unix, "/home");
        fs.add_file("/home/src/a.c", b"int main;");
        fs.add_file("/home/src/lib/b.h", b"#define B");
        fs.add_file("/home/notes.txt", b"hi");
        let local = tempfile::tempdir().unwrap();
        let picks = vec![
            Pick {
                name: "src".into(),
                is_dir: true,
            },
            Pick {
                name: "notes.txt".into(),
                is_dir: false,
            },
        ];
        let s = run(
            fs.clone(),
            Direction::Download,
            local.path().to_path_buf(),
            picks,
            false,
        );
        assert_eq!((s.files, s.bytes), (3, 9 + 9 + 2));
        assert_eq!(
            std::fs::read(local.path().join("src/lib/b.h")).unwrap(),
            b"#define B"
        );

        std::fs::write(local.path().join("new.txt"), b"fresh").unwrap();
        let picks = vec![
            Pick {
                name: "new.txt".into(),
                is_dir: false,
            },
            Pick {
                name: "notes.txt".into(),
                is_dir: false,
            },
        ];
        let s = run(
            fs.clone(),
            Direction::Upload,
            local.path().to_path_buf(),
            picks,
            false,
        );
        assert_eq!(s.files, 1);
        assert_eq!(s.skipped, ["notes.txt"]);
        let mut f = fs.clone();
        let mut got = String::new();
        f.open_read(&RemotePath::parse(PathStyle::Unix, "/home/new.txt"), 0)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "fresh");
    }
}
