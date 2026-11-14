//! HTTP/1.0 and Gopher against pretend servers on localhost.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;

use lc_core::{FileSession, PathStyle, RemotePath};

fn rp(p: &str) -> RemotePath {
    RemotePath::parse(PathStyle::Unix, p)
}

/// An NCSA-style HTTP/1.0 server: one request per connection.
fn httpd() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut range = None;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.strip_prefix("Range: bytes=") {
                    range = v.trim().trim_end_matches('-').parse::<usize>().ok();
                }
            }
            let path = first.split_whitespace().nth(1).unwrap().to_string();
            let head = first.starts_with("HEAD");
            let (status, body): (&str, Vec<u8>) = match path.as_str() {
                "/" => ("200 OK", b"<a href=\"pub/\">pub/</a>".to_vec()),
                "/pub" => ("301 Moved", Vec::new()),
                "/pub/" => ("200 OK", b"<PRE><A HREF=\"/\">Parent Directory</A>\n<A HREF=\"README\">README</A>  13-Nov-1997 10:04  1k\n<A HREF=\"gnu/\">gnu/</A>  12-Oct-1997 09:00  -\n</PRE>".to_vec()),
                "/pub/README" => ("200 OK", b"0123456789".to_vec()),
                _ => ("404 Not Found", b"no".to_vec()),
            };
            let (status, body) = match (range, path.as_str()) {
                (Some(n), "/pub/README") => ("206 Partial", body[n..].to_vec()),
                _ => (status, body),
            };
            let mut resp = format!(
                "HTTP/1.0 {status}\r\nServer: NCSA/1.5.2\r\nContent-Length: {}\r\n",
                body.len()
            );
            if path == "/pub" {
                resp.push_str(&format!("Location: http://127.0.0.1:{port}/pub/\r\n"));
            }
            resp.push_str("\r\n");
            s.write_all(resp.as_bytes()).unwrap();
            if !head {
                s.write_all(&body).unwrap();
            }
        }
    });
    port
}

#[test]
fn http_lists_index_pages_and_reads_files() {
    let port = httpd();
    let mut s =
        lc_http::http::connect("127.0.0.1", port, "", "", &lc_http::Options::default()).unwrap();
    assert_eq!(s.server, "NCSA/1.5.2");
    let root: Vec<_> = s
        .list(&rp("/"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(root, ["pub"]);
    let pubd = s.list(&rp("/pub")).unwrap();
    let names: Vec<_> = pubd
        .iter()
        .map(|e| (e.name.as_str(), e.is_folder()))
        .collect();
    assert_eq!(names, [("README", false), ("gnu", true)]);
    let mut text = String::new();
    s.open_read(&rp("/pub/README"), 4)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "456789");
    assert_eq!(s.stat(&rp("/pub/README")).unwrap().size, Some(10));
    assert!(matches!(
        s.list(&rp("/nope")),
        Err(lc_core::Error::NotFound(_))
    ));
}

/// A Gopher server with a root menu, a sub-menu and a text document.
fn gopherd() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut sel = String::new();
            BufReader::new(s.try_clone().unwrap())
                .read_line(&mut sel)
                .unwrap();
            let body = match sel.trim_end() {
                "" => format!(
                    "iWelcome to the hole\t\terror.host\t1\r\n1Software\t/software\t127.0.0.1\t{port}\r\n0About\t/about.txt\t127.0.0.1\t{port}\r\n1Elsewhere\t/\tgopher.floodgap.com\t70\r\n.\r\n"
                ),
                "/software" => format!("9gcc-2.95.tar.gz\t/software/gcc\t127.0.0.1\t{port}\r\n.\r\n"),
                "/about.txt" => "Hello from 1993.\r\n.\r\n".to_string(),
                "/software/gcc" => "\x1f\x0b binary".to_string(),
                _ => "3Not found\t\terror.host\t1\r\n.\r\n".to_string(),
            };
            s.write_all(body.as_bytes()).unwrap();
        }
    });
    port
}

#[test]
fn gopher_menus_are_folders() {
    let port = gopherd();
    let mut s = lc_http::gopher::connect("127.0.0.1", port, &lc_http::Options::default()).unwrap();
    let root: Vec<_> = s
        .list(&rp("/"))
        .unwrap()
        .into_iter()
        .map(|e| (e.name.clone(), e.is_folder()))
        .collect();
    assert_eq!(
        root,
        [("Software".to_string(), true), ("About".to_string(), false)]
    );
    let sw: Vec<_> = s
        .list(&rp("/Software"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(sw, ["gcc-2.95.tar.gz"]);
    let mut text = String::new();
    s.open_read(&rp("/About"), 0)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "Hello from 1993.\r\n");
    let mut bin = Vec::new();
    s.open_read(&rp("/Software/gcc-2.95.tar.gz"), 0)
        .unwrap()
        .read_to_end(&mut bin)
        .unwrap();
    assert_eq!(bin, b"\x1f\x0b binary");
}
