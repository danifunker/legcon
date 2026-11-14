//! Copying to and from a pretend AFP server keeps resource forks as AppleDouble sidecars (D31).

#[path = "../crates/lc-afp/tests/support/fake.rs"]
mod fake;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lc_core::{FileSession, MacExtras, PathStyle, RemotePath};
use legcon::model::appledouble;
use legcon::model::job::{Job, NoNotify};
use legcon::model::transfer::{copy, Batch, Direction, Pick, Summary};

fn run(
    s: &Arc<Mutex<lc_afp::AfpSession>>,
    dir: Direction,
    remote: &str,
    local: PathBuf,
    name: &str,
) -> Summary {
    let s = Arc::clone(s);
    let (remote, name) = (RemotePath::parse(PathStyle::Mac, remote), name.to_string());
    let job = Job::spawn("copy", Arc::new(NoNotify), move |ctx| {
        let picks = vec![Pick {
            name,
            is_dir: false,
        }];
        let b = Batch {
            dir,
            remote_dir: &remote,
            local_dir: &local,
            picks: &picks,
            sizes: &[None],
            overwrite: true,
        };
        copy(&mut *s.lock().unwrap(), &b, ctx)
    });
    job.wait();
    job.take_result().expect("the copy finished")
}

#[test]
fn forks_travel_as_appledouble_both_ways() {
    let f = fake::start(fake::sample());
    let s = lc_afp::connect(
        "127.0.0.1",
        f.port,
        "dani",
        "pw",
        &lc_afp::Options::default(),
    )
    .unwrap();
    let s = Arc::new(Mutex::new(s));
    let dir = tempfile::tempdir().unwrap();

    let sum = run(
        &s,
        Direction::Download,
        "Macintosh HD",
        dir.path().into(),
        "SimpleText\u{2122}",
    );
    assert_eq!(sum.files, 1, "{sum:?}");
    let side = std::fs::read(dir.path().join("._SimpleText\u{2122}")).unwrap();
    let x = appledouble::parse(&side).unwrap();
    assert_eq!(x.resource_fork, b"\0\0\x01\0resource fork bytes");
    assert_eq!(x.type_code(), *b"APPL");
    // A plain text file has no fork but keeps its type and creator.
    run(
        &s,
        Direction::Download,
        "Macintosh HD",
        dir.path().into(),
        "Read Me",
    );
    assert!(dir.path().join("._Read Me").is_file());

    let mut fi = [0u8; 32];
    fi[..8].copy_from_slice(b"APPLMOSS");
    let mine = MacExtras {
        finder_info: fi,
        resource_fork: vec![7; 5000],
    };
    std::fs::write(dir.path().join("Navigator"), b"data fork").unwrap();
    std::fs::write(dir.path().join("._Navigator"), appledouble::build(&mine)).unwrap();
    let sum = run(
        &s,
        Direction::Upload,
        "Macintosh HD:System Folder",
        dir.path().into(),
        "Navigator",
    );
    assert_eq!(sum.files, 1, "{sum:?}");
    let target = RemotePath::parse(PathStyle::Mac, "Macintosh HD:System Folder:Navigator");
    assert_eq!(s.lock().unwrap().mac_extras(&target).unwrap(), Some(mine));
    let names: Vec<String> = s
        .lock()
        .unwrap()
        .list(&RemotePath::parse(
            PathStyle::Mac,
            "Macintosh HD:System Folder",
        ))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["Navigator"]);
}
