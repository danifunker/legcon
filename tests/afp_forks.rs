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
    try_run(s, dir, remote, local, name).expect("the copy finished")
}

/// A batch where every file failed comes back as its error.
fn try_run(
    s: &Arc<Mutex<lc_afp::AfpSession>>,
    dir: Direction,
    remote: &str,
    local: PathBuf,
    name: &str,
) -> Result<Summary, String> {
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
    match job.wait().status {
        legcon::model::job::Status::Failed(e) => Err(e),
        _ => job.take_result().ok_or_else(|| "no result".into()),
    }
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
    // The machine's date (the pretend server says Nov 1997) stays on both files.
    let when = 946_684_800 - 68_000_000;
    assert_eq!(appledouble::modified(&side), Some(when));
    for f in ["SimpleText\u{2122}", "._SimpleText\u{2122}"] {
        let t = std::fs::metadata(dir.path().join(f))
            .unwrap()
            .modified()
            .unwrap();
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(secs, when, "{f}");
    }
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
    std::fs::write(
        dir.path().join("._Navigator"),
        appledouble::build(&mine, None),
    )
    .unwrap();
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

#[test]
fn names_windows_refuses_round_trip_and_bad_ones_are_explained() {
    let mut store = fake::sample();
    store.add("Macintosh HD:What?", false, b"q", b"", b"TEXTttxt");
    store.add("Macintosh HD:Q1/Q2", false, b"slash", b"", &[0; 8]);
    let f = fake::start(store);
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
    for name in ["What?", "Q1/Q2"] {
        let sum = run(
            &s,
            Direction::Download,
            "Macintosh HD",
            dir.path().into(),
            name,
        );
        assert_eq!(sum.files, 1, "{name}: {sum:?}");
    }
    assert_eq!(
        std::fs::read(dir.path().join("What\u{ff1f}")).unwrap(),
        b"q"
    );
    assert_eq!(
        std::fs::read(dir.path().join("Q1\u{ff0f}Q2")).unwrap(),
        b"slash"
    );
    assert!(dir.path().join("._What\u{ff1f}").is_file());

    // Back up under its own name, into another folder.
    let sum = run(
        &s,
        Direction::Upload,
        "Macintosh HD:System Folder",
        dir.path().into(),
        "Q1\u{ff0f}Q2",
    );
    assert_eq!(sum.files, 1, "{sum:?}");
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
    assert_eq!(names, ["Q1/Q2"]);

    let long = "A name much longer than classic Mac OS allows.txt";
    std::fs::write(dir.path().join(long), b"x").unwrap();
    std::fs::write(dir.path().join("\u{4e2d}\u{6587}.txt"), b"x").unwrap();
    for (name, why) in [
        (long, "allows 31"),
        ("\u{4e2d}\u{6587}.txt", "MacRoman cannot hold"),
    ] {
        // Every file in the batch failed, so the batch fails with the reason.
        let e = try_run(
            &s,
            Direction::Upload,
            "Macintosh HD",
            dir.path().into(),
            name,
        )
        .unwrap_err();
        assert!(e.contains(why), "{name}: {e}");
    }
}
