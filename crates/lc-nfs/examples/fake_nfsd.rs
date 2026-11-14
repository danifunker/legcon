//! A pretend NFS server for trying the app: `cargo run -p lc-nfs --example fake_nfsd [v2]`.
//! It prints its port; give the machine an NFS service on that port (a portmapper there).

#[path = "../tests/support/fake.rs"]
mod fake;

fn main() {
    let v3 = std::env::args().nth(1).as_deref() != Some("v2");
    let f = fake::start(fake::sample_fs(), v3);
    println!("{}", f.port);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let un = f.unmounted.lock().unwrap();
        if !un.is_empty() {
            println!("unmounted so far: {:?}", *un);
        }
    }
}
