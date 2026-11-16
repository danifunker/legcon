//! Transfer speed against a real server, in MB/s:
//! `cargo run --release -p lc-ssh --example bench -- HOST PORT USER KEYFILE REMOTE_DIR [MB]`.
//! Uploads and downloads a file of MB megabytes (default 200) over SFTP, then over SCP.

use std::io::{Read, Write};
use std::time::Instant;

use lc_core::{FileSession, PathStyle, RemotePath, WriteMode};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (host, port, user, key, dir) = (&a[0], a[1].parse().unwrap(), &a[2], &a[3], &a[4]);
    let mb: usize = a.get(5).map_or(200, |m| m.parse().unwrap());
    let data: Vec<u8> = (0..mb * 1024 * 1024)
        .map(|i| (i * 7 + i / 4096) as u8)
        .collect();
    for sftp in [true, false] {
        let opts = lc_ssh::Options {
            sftp,
            ..Default::default()
        };
        let auth = lc_ssh::Auth {
            key: Some(lc_ssh::KeyFiles {
                private: key.into(),
                public: format!("{key}.pub").into(),
            }),
            ..Default::default()
        };
        let mut s = lc_ssh::connect(host, port, user, &auth, &opts, &mut |_| Ok(())).unwrap();
        let what = if sftp { "SFTP" } else { "SCP " };
        let path = RemotePath::parse(PathStyle::Unix, &format!("{dir}/legcon-bench.bin"));
        let t = Instant::now();
        {
            let mut w = s
                .open_write_sized(&path, WriteMode::Overwrite, data.len() as u64)
                .unwrap();
            for chunk in data.chunks(64 * 1024) {
                w.write_all(chunk).unwrap();
            }
            w.flush().unwrap();
        }
        let up = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let mut back = Vec::with_capacity(data.len());
        s.open_read(&path, 0)
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        let down = t.elapsed().as_secs_f64();
        assert!(back == data, "{what}: the file came back different");
        let _ = s.remove(&path);
        println!(
            "{what} {} {} MB: up {:.1} MB/s, down {:.1} MB/s",
            s.negotiated().cipher,
            mb,
            mb as f64 / up,
            mb as f64 / down
        );
    }
}
