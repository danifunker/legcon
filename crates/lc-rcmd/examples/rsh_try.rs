//! Try rsh and rcp against a real (or pretend) machine, cleaning up after itself:
//! `cargo run -p lc-rcmd --example rsh_try -- HOST PORT LOCAL-USER REMOTE-USER`.

use std::io::{Read, Write};
use std::time::Instant;

use lc_core::{Charset, FileSession, WriteMode};
use lc_rcmd::rsh::Rsh;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let port = a[1].parse().unwrap();
    let opts = lc_rcmd::Options::default();
    let mut r = Rsh::connect(&a[0], port, &a[2], &a[3], Charset::Latin1, &opts).expect("rsh");
    let home = r.home().expect("home");
    let names: Vec<_> = r.list(&home).expect("list").into_iter().map(|e| e.name).collect();
    println!("{home}: {names:?}");
    let file = home.join("legcon-try.txt");
    let body: Vec<u8> = (0..100_000u32)
        .map(|i| b"legcon rcp test\n"[(i % 16) as usize])
        .collect();
    let t = Instant::now();
    r.open_write_sized(&file, WriteMode::Overwrite, body.len() as u64)
        .expect("open to write")
        .write_all(&body)
        .expect("write");
    println!("wrote {} bytes with rcp in {:?}", body.len(), t.elapsed());
    let moved = home.join("legcon-try-moved.txt");
    r.rename(&file, &moved).expect("rename");
    println!("stat: {:?}", r.stat(&moved).expect("stat").size);
    let mut back = Vec::new();
    r.open_read(&moved, 0)
        .expect("open to read")
        .read_to_end(&mut back)
        .expect("read");
    println!("read back {} bytes, equal {}", back.len(), back == body);
    r.remove(&moved).expect("delete");
    println!("console: {:?}", r.quote(&home, "echo hi from $0").expect("quote"));
}
