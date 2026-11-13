//! A pretend Windows 95 (or NT 4) file server for trying legcon by hand:
//! `cargo run -p lc-smb --example fake_smbd -- win95 1139` (share C, password "Secret").

#[path = "../tests/support/fake.rs"]
mod fake;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

fn main() {
    let mut args = std::env::args().skip(1);
    let kind = match args.next().as_deref() {
        Some("nt4") => fake::Kind::Nt4,
        _ => fake::Kind::Win95,
    };
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(1139);
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("the port is free");
    let disk = Arc::new(Mutex::new(fake::Disk::default()));
    fake::seed(&disk);
    println!("fake SMB server on 127.0.0.1:{port}");
    loop {
        fake::serve(kind, &listener, &disk);
    }
}
