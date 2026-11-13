//! A pretend wu-ftpd on localhost: login, PWD, a Latin-1 listing, download, upload, QUIT.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::{Charset, Connection, FileSession, WriteMode};

fn serve(listener: TcpListener, seen: Arc<Mutex<Vec<String>>>, stored: Arc<Mutex<Vec<u8>>>) {
    let (ctrl, _) = listener.accept().unwrap();
    ctrl.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut out = ctrl.try_clone().unwrap();
    let mut lines = BufReader::new(ctrl);
    out.write_all(b"220-sparc FTP server (Version wu-2.6.1(1)) ready.\r\n220 Welcome.\r\n")
        .unwrap();
    let mut data: Option<TcpListener> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if lines.read_until(b'\n', &mut line).unwrap() == 0 {
            return;
        }
        let text = String::from_utf8_lossy(&line).trim_end().to_string();
        seen.lock().unwrap().push(text.clone());
        let verb = text.split(' ').next().unwrap().to_string();
        let reply: &[u8] = match verb.as_str() {
            "USER" => b"331 Password required for dani.\r\n",
            "PASS" => b"230 User dani logged in.\r\n",
            "TYPE" => b"200 Type set to I.\r\n",
            "PWD" => b"257 \"/export/home/dani\" is current directory.\r\n",
            "CWD" => b"250 CWD command successful.\r\n",
            "PASV" => {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                let p = l.local_addr().unwrap().port();
                data = Some(l);
                // Like a server behind NAT, it reports an address the client must not use.
                let msg = format!(
                    "227 Entering Passive Mode (10,9,8,7,{},{})\r\n",
                    p / 256,
                    p % 256
                );
                out.write_all(msg.as_bytes()).unwrap();
                continue;
            }
            "LIST" | "RETR" | "STOR" => {
                out.write_all(b"150 Opening BINARY mode data connection.\r\n")
                    .unwrap();
                let (mut d, _) = data.take().unwrap().accept().unwrap();
                match verb.as_str() {
                    "LIST" => d
                        .write_all(b"total 2\r\ndrwxr-xr-x   2 dani  staff  512 Mar 16  1999 src\r\n-rw-r--r--   1 dani  staff   5 Nov 13 08:09 r\xe9sum\xe9.txt\r\n")
                        .unwrap(),
                    "RETR" => d.write_all(b"hello").unwrap(),
                    _ => {
                        let mut got = Vec::new();
                        d.read_to_end(&mut got).unwrap();
                        *stored.lock().unwrap() = got;
                    }
                }
                drop(d);
                b"226 Transfer complete.\r\n"
            }
            "QUIT" => {
                out.write_all(b"221 Goodbye.\r\n").unwrap();
                return;
            }
            _ => b"502 Command not implemented.\r\n",
        };
        out.write_all(reply).unwrap();
    }
}

#[test]
fn login_list_download_upload_quit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stored = Arc::new(Mutex::new(Vec::new()));
    let (s2, st2) = (Arc::clone(&seen), Arc::clone(&stored));
    let server = std::thread::spawn(move || serve(listener, s2, st2));

    let opts = lc_ftp::Options {
        charset: Charset::Latin1,
        now: Some(1_794_571_200),
        ..Default::default()
    };
    let mut ftp = lc_ftp::connect("127.0.0.1", port, "dani", "secret", &opts).unwrap();
    let home = ftp.home().unwrap();
    assert_eq!(home.to_string(), "/export/home/dani");
    let names: Vec<_> = ftp
        .list(&home)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["src", "r\u{e9}sum\u{e9}.txt"]);
    let mut got = String::new();
    ftp.open_read(&home.join("r\u{e9}sum\u{e9}.txt"), 0)
        .unwrap()
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, "hello");
    {
        let mut w = ftp
            .open_write(&home.join("new.txt"), WriteMode::Create)
            .unwrap();
        w.write_all(b"uploaded").unwrap();
    }
    ftp.disconnect(Duration::from_secs(2)).unwrap();
    server.join().unwrap();
    assert_eq!(*stored.lock().unwrap(), b"uploaded");
    let seen = seen.lock().unwrap();
    assert!(seen.contains(&"PASS secret".to_string()));
    assert!(seen
        .iter()
        .any(|l| l.starts_with("RETR /export/home/dani/r")));
    assert!(
        !seen.iter().any(|l| l.starts_with("PORT")),
        "never active mode"
    );
    assert_eq!(seen.last().map(String::as_str), Some("QUIT"));
    let trace = ftp.trace();
    let trace = trace.lock().unwrap();
    assert!(
        trace.contains(&lc_ftp::Trace::Sent("PASS ********".into())),
        "the trace hides the password"
    );
}
