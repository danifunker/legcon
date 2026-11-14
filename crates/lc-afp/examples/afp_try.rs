//! Try AFP against a real server, cleaning up after itself:
//! `cargo run -p lc-afp --example afp_try -- HOST USER PASSWORD [VOLUME]`.
//! It lists the volume, then writes, renames, reads back and deletes `legcon-try` at its top.
//! `... HOST USER PASSWORD --rm Volume:Path` only deletes that one item.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use lc_core::{Connection, FileSession, PathStyle, RemotePath, WriteMode};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let t = Instant::now();
    let mut s = lc_afp::connect(
        &a[0],
        lc_afp::PORT,
        &a[1],
        &a[2],
        &lc_afp::Options::default(),
    )
    .expect("connect and log in");
    println!(
        "server {:?} ({}), versions {:?}, logins {:?}",
        s.info.name, s.info.machine, s.info.versions, s.info.uams
    );
    println!("using {} with {} ({:?})", s.version, s.uam, t.elapsed());
    println!("volumes: {:?}", s.volumes());
    if a.get(3).map(String::as_str) == Some("--rm") {
        let p = RemotePath::parse(PathStyle::Mac, &a[4]);
        s.remove(&p).expect("delete");
        println!("deleted {p}");
        s.disconnect(Duration::from_secs(5)).expect("log out");
        return;
    }
    let vol = a
        .get(3)
        .cloned()
        .unwrap_or_else(|| s.volumes().first().cloned().expect("a volume"));
    let root = RemotePath::parse(PathStyle::Mac, &vol);
    let t = Instant::now();
    let list = s.list(&root).expect("list");
    println!("{vol}: {} items in {:?}", list.len(), t.elapsed());
    for e in list.iter().take(12) {
        println!(
            "  {:<32} {:>9} {} {:?}{}",
            e.name,
            e.size.map(|n| n.to_string()).unwrap_or_default(),
            if e.is_folder() { "dir " } else { "file" },
            e.mac_type_creator
                .map(|(t, c)| format!(
                    "{}/{}",
                    String::from_utf8_lossy(&t),
                    String::from_utf8_lossy(&c)
                ))
                .unwrap_or_default(),
            if e.has_resource_fork { " +rsrc" } else { "" }
        );
        if let Some(m) = e.modified {
            println!("      modified {} (unix seconds)", m.unix);
        }
    }
    if let Some(f) = list.iter().find(|e| e.has_resource_fork) {
        let x = s.mac_extras(&root.join(&f.name)).expect("forks");
        println!(
            "resource fork of {:?}: {} bytes",
            f.name,
            x.map(|x| x.resource_fork.len()).unwrap_or(0)
        );
    }
    let file = root.join("legcon-try");
    let body: Vec<u8> = (0..100_000u32)
        .map(|i| b"legcon AFP test\r"[(i % 16) as usize])
        .collect();
    let t = Instant::now();
    s.open_write(&file, WriteMode::Overwrite)
        .expect("open to write")
        .write_all(&body)
        .expect("write");
    println!("wrote {} bytes in {:?}", body.len(), t.elapsed());
    let moved = root.join("legcon-try moved");
    s.rename(&file, &moved).expect("rename");
    let mut back = Vec::new();
    s.open_read(&moved, 0)
        .expect("open to read")
        .read_to_end(&mut back)
        .expect("read");
    println!("read back {} bytes, equal {}", back.len(), back == body);
    s.remove(&moved).expect("delete");
    let left: Vec<_> = s
        .list(&root)
        .expect("list")
        .into_iter()
        .filter(|e| e.name.starts_with("legcon-try"))
        .map(|e| e.name)
        .collect();
    println!("left behind: {left:?}");
    s.disconnect(Duration::from_secs(5)).expect("log out");
    println!("logged out");
}
