//! Try every SMB operation against a real machine, cleaning up after itself:
//! `cargo run -p lc-smb --example smb_try -- HOST USER PASSWORD SHARE\\FOLDER [FILE-TO-READ]`.
//! It writes, renames, reads back and deletes `legcon-try.txt` and `legcon-try` in FOLDER.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use lc_core::{Connection, FileSession, PathStyle, RemotePath, WriteMode};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (host, user, password, folder) = (&a[0], &a[1], &a[2], &a[3]);
    let t = Instant::now();
    let mut s = lc_smb::connect(
        host,
        lc_smb::PORT,
        user,
        password,
        &lc_smb::Options::default(),
    )
    .expect("connect and log in");
    let n = s.negotiated().clone();
    println!(
        "server {} dialect {} user-level {} encrypted {} max buffer {} domain {:?} guest {} ({:?})",
        s.server(),
        n.dialect,
        n.user_level,
        n.encrypt,
        n.max_buffer,
        n.domain,
        s.guest,
        t.elapsed()
    );
    let dir = RemotePath::parse(PathStyle::Dos, folder);
    if let Some(f) = a.get(4) {
        let p = RemotePath::parse(PathStyle::Dos, f);
        let mut text = Vec::new();
        s.open_read(&p, 0)
            .expect("open to read")
            .read_to_end(&mut text)
            .expect("read");
        println!(
            "read {p}: {} bytes, starts {:?}",
            text.len(),
            String::from_utf8_lossy(&text[..text.len().min(60)])
        );
    }
    let file = dir.join("legcon-try.txt");
    let body: Vec<u8> = (0..100_000u32)
        .map(|i| b"legcon SMB test\r\n"[(i % 17) as usize])
        .collect();
    let t = Instant::now();
    s.open_write(&file, WriteMode::Overwrite)
        .expect("open to write")
        .write_all(&body)
        .expect("write");
    println!("wrote {file}: {} bytes in {:?}", body.len(), t.elapsed());
    s.mkdir(&dir.join("legcon-try")).expect("mkdir");
    let moved = dir.join("legcon-try").join("moved.txt");
    s.rename(&file, &moved).expect("rename");
    let mut back = Vec::new();
    s.open_read(&moved, 0)
        .expect("open moved")
        .read_to_end(&mut back)
        .expect("read back");
    println!(
        "read back after rename: {} bytes, equal {}",
        back.len(),
        back == body
    );
    let st = s.stat(&moved).expect("stat");
    println!(
        "stat: size {:?} attrs {:?} modified {:?}",
        st.size, st.dos_attributes, st.modified
    );
    s.remove(&moved).expect("delete file");
    s.remove(&dir.join("legcon-try")).expect("delete folder");
    let left: Vec<String> = s
        .list(&dir)
        .expect("list")
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n.starts_with("legcon-try"))
        .collect();
    println!("left behind: {left:?}");
    s.disconnect(Duration::from_secs(5)).expect("log off");
    println!("logged off");
}
