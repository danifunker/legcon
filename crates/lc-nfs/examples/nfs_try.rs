//! Connect to an NFS server and list each export: `nfs_try host [portmap-port] [2|3] [udp|tcp]`.
use lc_core::FileSession;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let opts = lc_nfs::Options {
        uid: 1000,
        gid: 1000,
        portmap_port: a.get(2).map_or(111, |p| p.parse().unwrap()),
        version: a.get(3).map(|v| v.parse().unwrap()),
        tcp: a.get(4).map(|t| t == "tcp"),
        ..Default::default()
    };
    let mut s = match lc_nfs::connect(&a[1], &opts) {
        Ok(s) => s,
        Err(e) => return eprintln!("connect: {e:?}"),
    };
    println!("{} {:?}", s.describe(), s.exports());
    let root = lc_core::RemotePath::root(lc_core::PathStyle::Unix);
    match s.list(&root) {
        Ok(l) => l.iter().for_each(|e| println!("  {}", e.name)),
        Err(e) => eprintln!("list: {e:?}"),
    }
}
