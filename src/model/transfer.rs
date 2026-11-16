//! Copying between this computer and a machine, folders included, for the transfer queue.
//!
//! [`plan`] walks a batch's folders (listing the remote side, reading the local one) into one
//! [`Item`] per file or folder and notes which targets exist; [`copy_item`] then copies one item
//! in 64 KB blocks, resuming where asked, honours cancel between blocks, and checks the result by
//! size and, where the machine can compute one, a checksum.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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

/// One queue entry: a file to copy, or a folder to make on the target side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub dir: Direction,
    pub remote: RemotePath,
    pub local: PathBuf,
    pub size: Option<u64>,
    /// The remote file's modification time (seconds since 1970), when known.
    pub modified: Option<i64>,
    /// A folder to create on the target side, not a file to copy.
    pub folder: bool,
    /// The target was already there when the batch was planned, and its size if known.
    pub exists: Option<Option<u64>>,
    /// Why it cannot be copied at all (a name the machine refuses), found while planning.
    pub problem: Option<String>,
}

impl Item {
    /// The folder the copy goes into, as text: the key for the names already there.
    pub fn target_folder(&self) -> String {
        match self.dir {
            Direction::Download => self
                .local
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            Direction::Upload => self
                .remote
                .parent()
                .map(|p| p.to_string())
                .unwrap_or_default(),
        }
    }

    /// The name the copy gets on the target side.
    pub fn target_name(&self) -> String {
        match self.dir {
            Direction::Download => self
                .local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Direction::Upload => self.remote.name().to_string(),
        }
    }

    /// Copy under another name in the same folder, which is not there yet.
    pub fn rename_target(&mut self, name: &str) {
        match self.dir {
            Direction::Download => self.local = self.local.with_file_name(name),
            Direction::Upload => {
                if let Some(p) = self.remote.parent() {
                    self.remote = p.join(name);
                }
            }
        }
        self.exists = None;
    }

    /// The name the queue shows: the source side's.
    pub fn name(&self) -> String {
        match self.dir {
            Direction::Download => self.remote.name().to_string(),
            Direction::Upload => self
                .local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

type Step = Item;

fn step(dir: Direction, remote: RemotePath, local: PathBuf, folder: bool) -> Step {
    Item {
        dir,
        remote,
        local,
        size: None,
        modified: None,
        folder,
        exists: None,
        problem: None,
    }
}

fn walk_remote(
    fs: &mut dyn FileSession,
    remote: RemotePath,
    local: PathBuf,
    out: &mut Vec<Step>,
    ctx: &JobCtx,
) -> anyhow::Result<()> {
    ctx.check_cancel()?;
    out.push(step(
        Direction::Download,
        remote.clone(),
        local.clone(),
        true,
    ));
    for e in fs.list(&remote)? {
        let (r, l) = (
            remote.join(&e.name),
            local.join(super::names::to_local(&e.name)),
        );
        if e.is_folder() {
            walk_remote(fs, r, l, out, ctx)?;
        } else if e.kind == lc_core::EntryKind::File || e.kind == lc_core::EntryKind::Other {
            out.push(Item {
                size: e.size,
                modified: e.modified.map(|t| t.unix),
                ..step(Direction::Download, r, l, false)
            });
        }
    }
    Ok(())
}

fn walk_local(
    local: PathBuf,
    remote: RemotePath,
    out: &mut Vec<Step>,
    names: &dyn Fn(&str) -> String,
    ctx: &JobCtx,
) -> anyhow::Result<()> {
    ctx.check_cancel()?;
    out.push(step(Direction::Upload, remote.clone(), local.clone(), true));
    for item in std::fs::read_dir(&local).with_context(|| format!("reading {}", local.display()))? {
        let item = item?;
        let name = names(&item.file_name().to_string_lossy());
        let meta = item.metadata()?;
        if meta.is_dir() {
            walk_local(item.path(), remote.join(&name), out, names, ctx)?;
        } else {
            out.push(Item {
                size: Some(meta.len()),
                ..step(Direction::Upload, remote.join(&name), item.path(), false)
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
}

/// A batch walked into queue items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub items: Vec<Item>,
    /// The protocol can continue a partly copied file in this direction.
    pub can_resume: bool,
    /// Names already in each target folder that holds a clash ([`Item::target_folder`]).
    pub taken: HashMap<String, Vec<String>>,
}

impl Plan {
    pub fn files(&self) -> usize {
        self.items.iter().filter(|i| !i.folder).count()
    }

    pub fn bytes(&self) -> u64 {
        self.items.iter().filter_map(|i| i.size).sum()
    }
}

fn sidecar_of(local: &Path) -> Option<PathBuf> {
    let name = local.file_name()?.to_string_lossy().into_owned();
    Some(local.with_file_name(super::appledouble::sidecar_name(&name)))
}

fn system_time(unix: i64) -> Option<std::time::SystemTime> {
    let base = std::time::UNIX_EPOCH;
    if unix >= 0 {
        base.checked_add(std::time::Duration::from_secs(unix as u64))
    } else {
        base.checked_sub(std::time::Duration::from_secs(unix.unsigned_abs()))
    }
}

/// After a download: the file's resource fork and Finder info as an AppleDouble `._name`.
fn save_sidecar(
    fs: &mut dyn FileSession,
    remote: &RemotePath,
    local: &Path,
    modified: Option<i64>,
) -> anyhow::Result<()> {
    let Some(x) = fs.mac_extras(remote)? else {
        return Ok(());
    };
    let Some(side) = sidecar_of(local) else {
        return Ok(());
    };
    let side_file = std::fs::write(&side, super::appledouble::build(&x, modified));
    if let (Ok(()), Some(t)) = (&side_file, modified.and_then(system_time)) {
        let _ = File::options()
            .write(true)
            .open(&side)
            .and_then(|f| f.set_modified(t));
    }
    side_file.with_context(|| format!("writing {}", side.display()))
}

/// After an upload: a `._name` next to the local file becomes its fork and Finder info there.
fn apply_sidecar(
    fs: &mut dyn FileSession,
    local: &Path,
    remote: &RemotePath,
) -> anyhow::Result<()> {
    let Some(side) = sidecar_of(local).filter(|p| p.is_file()) else {
        return Ok(());
    };
    let data = std::fs::read(&side).with_context(|| format!("reading {}", side.display()))?;
    match super::appledouble::parse(&data) {
        Some(x) => Ok(fs.set_mac_extras(remote, &x)?),
        None => Ok(()),
    }
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

/// Walk a batch into queue items, noting which targets already exist.
pub fn plan(fs: &mut dyn FileSession, b: &Batch, ctx: &JobCtx) -> anyhow::Result<Plan> {
    let (dir, remote_dir, local_dir) = (b.dir, b.remote_dir, b.local_dir);
    ctx.set_phase("Looking at what to copy");
    let (charset, style) = (fs.charset(), remote_dir.style);
    let to_remote = move |n: &str| super::names::to_remote(n, charset, style);
    let mut steps = Vec::new();
    for (p, size) in b.picks.iter().zip(b.sizes) {
        // A pick is named as its own side shows it; the other side gets the mapped name.
        let (r, l) = match dir {
            Direction::Download => (
                remote_dir.join(&p.name),
                local_dir.join(super::names::to_local(&p.name)),
            ),
            Direction::Upload => (
                remote_dir.join(&to_remote(&p.name)),
                local_dir.join(&p.name),
            ),
        };
        match (dir, p.is_dir) {
            (Direction::Download, true) => walk_remote(fs, r, l, &mut steps, ctx)?,
            (Direction::Upload, true) => walk_local(l, r, &mut steps, &to_remote, ctx)?,
            _ => {
                // A picked file's date (and size, if the pane had none) come from the machine.
                let st = match dir {
                    Direction::Download => fs.stat(&r).ok(),
                    Direction::Upload => None,
                };
                steps.push(Item {
                    size: size.or(st.as_ref().and_then(|e| e.size)),
                    modified: st.and_then(|e| e.modified).map(|t| t.unix),
                    ..step(dir, r, l, false)
                })
            }
        }
    }
    // Sidecars travel with their file into a protocol that keeps forks, not as files of their own.
    if fs.keeps_mac_extras() && dir == Direction::Upload {
        steps.retain(|s| {
            let name = s
                .local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            !name.is_some_and(|n| super::appledouble::is_sidecar(&n))
        });
    }
    ctx.set_phase("Looking at what is already there");
    // Upload targets: each remote folder is listed once; one found missing needs no listing below.
    let mut there: HashMap<String, Vec<(String, Option<u64>)>> = HashMap::new();
    let max_len = fs.max_name_len();
    for s in &mut steps {
        ctx.check_cancel()?;
        match dir {
            Direction::Download => {
                if !s.folder {
                    s.exists = std::fs::metadata(&s.local)
                        .ok()
                        .filter(|m| !m.is_dir())
                        .map(|m| Some(m.len()));
                }
            }
            Direction::Upload => {
                let Some(parent) = s.remote.parent() else {
                    continue;
                };
                let key = parent.to_string();
                if !there.contains_key(&key) {
                    let list = fs
                        .list(&parent)
                        .map(|l| l.into_iter().map(|e| (e.name, e.size)).collect())
                        .unwrap_or_default();
                    there.insert(key.clone(), list);
                }
                let name = s.remote.name().to_string();
                let found = there[&key].iter().find(|(n, _)| *n == name).map(|e| e.1);
                if s.folder {
                    if found.is_none() {
                        there.insert(s.remote.to_string(), Vec::new());
                    }
                } else {
                    s.exists = found;
                    s.problem = super::names::remote_problem(&name, charset, max_len);
                }
            }
        }
    }
    // The names in each folder where something clashes, so a new name can be offered.
    let mut taken: HashMap<String, Vec<String>> = HashMap::new();
    for s in steps.iter().filter(|s| s.exists.is_some()) {
        let key = s.target_folder();
        if taken.contains_key(&key) {
            continue;
        }
        let names = match dir {
            Direction::Download => s
                .local
                .parent()
                .and_then(|p| std::fs::read_dir(p).ok())
                .map(|d| {
                    d.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default(),
            Direction::Upload => there
                .get(&key)
                .map(|l| l.iter().map(|e| e.0.clone()).collect())
                .unwrap_or_default(),
        };
        taken.insert(key, names);
    }
    let caps = fs.caps();
    Ok(Plan {
        items: steps,
        can_resume: match dir {
            Direction::Download => true,
            Direction::Upload => caps.resume_upload,
        },
        taken,
    })
}

/// A whole batch in one job, without the queue (tests, scripts): existing targets are
/// overwritten or skipped.
pub fn copy(
    fs: &mut dyn FileSession,
    b: &Batch,
    overwrite: bool,
    ctx: &JobCtx,
) -> anyhow::Result<Summary> {
    let plan = plan(fs, b, ctx)?;
    let mut sum = Summary::default();
    for item in &plan.items {
        if item.exists.is_some() && !overwrite && !item.folder {
            sum.skipped.push(item.name());
            continue;
        }
        match copy_item(fs, item, Mode::Overwrite, false, ctx) {
            Ok(c) if !item.folder => {
                sum.files += 1;
                sum.bytes += c.bytes;
            }
            Ok(_) => {}
            Err(e) if e.downcast_ref::<super::job::Cancelled>().is_some() => return Err(e),
            Err(e) => sum.failed.push((item.name(), format!("{e:#}"))),
        }
    }
    if plan.files() > 0 && sum.files == 0 && !sum.failed.is_empty() && sum.skipped.is_empty() {
        bail!("nothing was copied: {}", sum.failed[0].1);
    }
    Ok(sum)
}

/// How to treat a target that is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Overwrite,
    /// Continue from the target's length (from the start where the protocol cannot).
    Resume,
    /// Leave a target that is the same file ([`same_as_target`]); overwrite one that is not.
    IfDifferent,
}

/// How a finished file was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// The sizes match and the machine computed a matching sum.
    Sum(lc_core::Checksum),
    Size(u64),
    /// No size to compare (a folder, or a machine that gives none).
    Unchecked,
}

/// One item copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// Not copied: the target is already this file, and how that was found.
    pub same: Option<String>,
    pub bytes: u64,
    /// Where it continued from, when it resumed.
    pub resumed_at: u64,
    pub verified: Verified,
}

/// The copy arrived but does not match its source; a retry starts it over rather than resuming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch(pub String);

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Mismatch {}

/// Copy one queue item, then check it by size and, when asked and the machine can, a checksum.
pub fn copy_item(
    fs: &mut dyn FileSession,
    item: &Item,
    mode: Mode,
    sum: bool,
    ctx: &JobCtx,
) -> anyhow::Result<Copied> {
    ctx.check_cancel()?;
    if item.folder {
        match item.dir {
            Direction::Download => std::fs::create_dir_all(&item.local)
                .with_context(|| format!("creating {}", item.local.display()))?,
            // An existing folder is fine; anything else shows up when its files fail.
            Direction::Upload => {
                let _ = fs.mkdir(&item.remote);
            }
        }
        return Ok(Copied {
            same: None,
            bytes: 0,
            resumed_at: 0,
            verified: Verified::Unchecked,
        });
    }
    if let Some(why) = &item.problem {
        bail!("{why}");
    }
    if mode == Mode::IfDifferent && item.exists.is_some() {
        if let Some(how) = same_as_target(fs, item, ctx)? {
            return Ok(Copied {
                same: Some(how),
                bytes: 0,
                resumed_at: 0,
                verified: Verified::Unchecked,
            });
        }
    }
    ctx.set_phase(item.name());
    let forks = fs.keeps_mac_extras();
    let (bytes, at, size) = match item.dir {
        Direction::Download => {
            if let Some(parent) = item.local.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let have = match mode {
                Mode::Resume => std::fs::metadata(&item.local).map_or(0, |m| m.len()),
                Mode::Overwrite | Mode::IfDifferent => 0,
            };
            let at = item.size.map_or(have, |s| have.min(s));
            let mut src = fs.open_read(&item.remote, at)?;
            let mut dst = File::options()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&item.local)
                .with_context(|| format!("creating {}", item.local.display()))?;
            dst.set_len(at)?;
            dst.seek(SeekFrom::End(0))?;
            let mut done = at;
            let n = pump(&mut src, &mut dst, &mut done, item.size.unwrap_or(0), ctx)?;
            drop(src);
            if let Some(t) = item.modified.and_then(system_time) {
                // Keeping the machine's date matters more than the copy itself failing over it.
                let _ = dst.set_modified(t);
            }
            drop(dst);
            if forks {
                save_sidecar(fs, &item.remote, &item.local, item.modified)?;
            }
            (n, at, item.size)
        }
        Direction::Upload => {
            let mut src = File::open(&item.local)
                .with_context(|| format!("opening {}", item.local.display()))?;
            let size = src.metadata()?.len();
            let mut at = 0;
            if mode == Mode::Resume && fs.caps().resume_upload {
                at = fs
                    .stat(&item.remote)
                    .ok()
                    .and_then(|e| e.size)
                    .filter(|&n| n <= size)
                    .unwrap_or(0);
            }
            let n = if at == size && at > 0 {
                0
            } else {
                match send_from(fs, &item.remote, &mut src, at, size, ctx) {
                    // A server may refuse to append (proftpd without AllowStoreRestart): send it whole.
                    Err(Sent::Refused(_)) if at > 0 => {
                        at = 0;
                        send_from(fs, &item.remote, &mut src, 0, size, ctx).map_err(Sent::error)?
                    }
                    r => r.map_err(Sent::error)?,
                }
            };
            if forks {
                apply_sidecar(fs, &item.local, &item.remote)?;
            }
            (n, at, Some(size))
        }
    };
    ctx.set_phase(format!("Checking {}", item.name()));
    let verified = verify(fs, item, size, sum)?;
    Ok(Copied {
        same: None,
        bytes,
        resumed_at: at,
        verified,
    })
}

/// Whether the file on the target side is already this one, without copying it: the sizes
/// match, then the machine's own checksum (`cksum`, `XCRC`) matches ours, or, where the machine
/// makes none, the dates match. `Some(how)` when it is the same.
pub fn same_as_target(
    fs: &mut dyn FileSession,
    item: &Item,
    ctx: &JobCtx,
) -> anyhow::Result<Option<String>> {
    ctx.set_phase(format!("Comparing {}", item.name()));
    let here = std::fs::metadata(&item.local).ok().filter(|m| m.is_file());
    let there = fs.stat(&item.remote).ok();
    let (Some(here), Some(there)) = (here, there) else {
        return Ok(None);
    };
    if there.size != Some(here.len()) {
        return Ok(None);
    }
    ctx.check_cancel()?;
    if let Ok(Some(theirs)) = fs.checksum(&item.remote) {
        let ours = theirs.of(&mut File::open(&item.local)?)?;
        return Ok((ours == theirs).then(|| format!("{} matches", theirs.name())));
    }
    let here_time = here
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    Ok(match (here_time, there.modified.map(|t| t.unix)) {
        // FAT keeps dates to two seconds.
        (Some(a), Some(b)) if (a - b).abs() <= 2 => Some("same size and date".into()),
        _ => None,
    })
}

/// Why an upload stopped: the machine would not open the target, or the copy failed after.
enum Sent {
    Refused(anyhow::Error),
    Failed(anyhow::Error),
}

impl Sent {
    fn error(self) -> anyhow::Error {
        match self {
            Sent::Refused(e) | Sent::Failed(e) => e,
        }
    }
}

/// Upload `src` from `at` on, appending to what the machine has when `at` is not 0.
fn send_from(
    fs: &mut dyn FileSession,
    remote: &RemotePath,
    src: &mut File,
    at: u64,
    size: u64,
    ctx: &JobCtx,
) -> Result<u64, Sent> {
    src.seek(SeekFrom::Start(at))
        .map_err(|e| Sent::Failed(e.into()))?;
    let mode = if at > 0 {
        WriteMode::Resume(at)
    } else {
        WriteMode::Overwrite
    };
    let mut dst = fs
        .open_write_sized(remote, mode, size - at)
        .map_err(|e| Sent::Refused(e.into()))?;
    let mut done = at;
    let n = pump(src, &mut dst, &mut done, size, ctx).map_err(Sent::Failed)?;
    drop(dst);
    Ok(n)
}

fn verify(
    fs: &mut dyn FileSession,
    item: &Item,
    size: Option<u64>,
    sum: bool,
) -> anyhow::Result<Verified> {
    let here = std::fs::metadata(&item.local)?.len();
    let there = match item.dir {
        Direction::Download => size,
        Direction::Upload => fs.stat(&item.remote).ok().and_then(|e| e.size),
    };
    let Some(there) = there else {
        return Ok(Verified::Unchecked);
    };
    if here != there {
        return Err(Mismatch(format!(
            "sizes differ: {here} bytes here, {there} on the machine"
        ))
        .into());
    }
    if sum {
        if let Ok(Some(theirs)) = fs.checksum(&item.remote) {
            let mut f = File::open(&item.local)?;
            let ours = theirs.of(&mut f)?;
            if ours != theirs {
                return Err(Mismatch(format!(
                    "{} differs: {ours} here, {theirs} on the machine",
                    theirs.name()
                ))
                .into());
            }
            return Ok(Verified::Sum(theirs));
        }
    }
    Ok(Verified::Size(here))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::job::{Job, NoNotify};
    use lc_core::memory::MemoryFs;
    use lc_core::PathStyle;
    use std::sync::{Arc, Mutex};

    fn in_job<R: Send + 'static>(
        fs: &MemoryFs,
        f: impl FnOnce(&mut MemoryFs, &JobCtx) -> anyhow::Result<R> + Send + 'static,
    ) -> anyhow::Result<R> {
        let fs = Arc::new(Mutex::new(fs.clone()));
        let job = Job::spawn("t", Arc::new(NoNotify), move |ctx| {
            f(&mut fs.lock().unwrap(), ctx)
        });
        job.wait();
        job.take_result()
            .ok_or_else(|| anyhow::anyhow!("{:?}", job.snapshot().status))
    }

    fn pick(name: &str, is_dir: bool) -> Pick {
        Pick {
            name: name.into(),
            is_dir,
        }
    }

    fn plan_of(fs: &MemoryFs, dir: Direction, local: &Path, picks: Vec<Pick>) -> Plan {
        let local = local.to_path_buf();
        in_job(fs, move |fs, ctx| {
            let sizes = vec![None; picks.len()];
            let home = RemotePath::parse(PathStyle::Unix, "/home");
            let b = Batch {
                dir,
                remote_dir: &home,
                local_dir: &local,
                picks: &picks,
                sizes: &sizes,
            };
            plan(fs, &b, ctx)
        })
        .unwrap()
    }

    fn run(fs: &MemoryFs, item: &Item, mode: Mode) -> anyhow::Result<Copied> {
        let item = item.clone();
        in_job(fs, move |fs, ctx| copy_item(fs, &item, mode, true, ctx))
    }

    fn remote(fs: &MemoryFs, path: &str) -> Vec<u8> {
        let mut out = Vec::new();
        fs.clone()
            .open_read(&RemotePath::parse(PathStyle::Unix, path), 0)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn download_a_folder_then_upload_with_one_already_there() {
        let fs = MemoryFs::new(PathStyle::Unix, "/home");
        fs.add_file("/home/src/a.c", b"int main;");
        fs.add_file("/home/src/lib/b.h", b"#define B");
        fs.add_file("/home/notes.txt", b"hi");
        let local = tempfile::tempdir().unwrap();
        let p = plan_of(
            &fs,
            Direction::Download,
            local.path(),
            vec![pick("src", true), pick("notes.txt", false)],
        );
        assert_eq!((p.files(), p.items.len()), (3, 5));
        assert!(p.items.iter().all(|i| i.exists.is_none()));
        for i in &p.items {
            let c = run(&fs, i, Mode::Overwrite).unwrap();
            if !i.folder {
                assert!(matches!(c.verified, Verified::Sum(_)), "{c:?}");
            }
        }
        assert_eq!(
            std::fs::read(local.path().join("src/lib/b.h")).unwrap(),
            b"#define B"
        );

        std::fs::write(local.path().join("new.txt"), b"fresh").unwrap();
        let p = plan_of(
            &fs,
            Direction::Upload,
            local.path(),
            vec![
                pick("new.txt", false),
                pick("notes.txt", false),
                pick("src", true),
            ],
        );
        let exists: Vec<(String, Option<Option<u64>>)> = p
            .items
            .iter()
            .filter(|i| !i.folder)
            .map(|i| (i.name(), i.exists))
            .collect();
        assert!(exists.contains(&("new.txt".into(), None)));
        assert!(exists.contains(&("notes.txt".into(), Some(Some(2)))));
        assert!(exists.contains(&("b.h".into(), Some(Some(9)))));
        assert!(p.can_resume);
        run(&fs, &p.items[0], Mode::Overwrite).unwrap();
        assert_eq!(remote(&fs, "/home/new.txt"), b"fresh");
    }

    #[test]
    fn resume_both_ways_and_catch_a_short_copy() {
        let fs = MemoryFs::new(PathStyle::Unix, "/home");
        fs.add_file("/home/big.bin", b"0123456789");
        let local = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("big.bin"), b"0123").unwrap();
        let p = plan_of(
            &fs,
            Direction::Download,
            local.path(),
            vec![pick("big.bin", false)],
        );
        let mut item = p.items[0].clone();
        assert_eq!(item.exists, Some(Some(4)));
        item.size = Some(10);
        let c = run(&fs, &item, Mode::Resume).unwrap();
        assert_eq!((c.resumed_at, c.bytes), (4, 6));
        assert_eq!(
            std::fs::read(local.path().join("big.bin")).unwrap(),
            b"0123456789"
        );

        // The machine claimed more than it sent: the size check fails and says so.
        item.size = Some(12);
        let e = run(&fs, &item, Mode::Overwrite).unwrap_err();
        assert!(e.to_string().contains("sizes differ"), "{e:#}");

        std::fs::write(local.path().join("up.txt"), b"abcdefgh").unwrap();
        fs.add_file("/home/up.txt", b"abc");
        let p = plan_of(
            &fs,
            Direction::Upload,
            local.path(),
            vec![pick("up.txt", false)],
        );
        let c = run(&fs, &p.items[0], Mode::Resume).unwrap();
        assert_eq!((c.resumed_at, c.bytes), (3, 5));
        assert_eq!(remote(&fs, "/home/up.txt"), b"abcdefgh");
        let c = run(&fs, &p.items[0], Mode::Overwrite).unwrap();
        assert_eq!((c.resumed_at, c.bytes), (0, 8));
    }
}
