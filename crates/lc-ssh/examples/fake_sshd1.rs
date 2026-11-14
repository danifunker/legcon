//! A pretend SSH-1 server for trying legcon by hand: `cargo run -p lc-ssh --example fake_sshd1 -- 2222`.
//! Password `secret`; Set up key login teaches it a key.

#[path = "../tests/support/ssh1_server.rs"]
mod ssh1_server;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(2222);
    ssh1_server::PORT.with(|p| *p.borrow_mut() = port);
    let (port, _server) = ssh1_server::start(None);
    println!("SSH-1 server on 127.0.0.1:{port} (password: secret)");
    loop {
        std::thread::park();
    }
}
