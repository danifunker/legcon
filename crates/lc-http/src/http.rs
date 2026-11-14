//! HTTP/1.0 browsing: a folder is its index page's links, files are GETs.
//!
//! One request per connection, as HTTP/1.0 servers expect. `Range` is asked for when resuming;
//! a server that ignores it sends the whole file and the start is skipped. Basic auth is sent
//! when the machine has a user. Nothing is written: PUT and WebDAV are not offered.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use lc_core::listing::days_from_civil;
use lc_core::{
    ConnState, Connection, Entry, EntryKind, Error, FileCaps, FileSession, PathStyle, RemotePath,
    Result, TimePrecision, Timestamp, WriteMode,
};

use crate::{dial, part_name, read_only, Options};

pub const PORT: u16 = 80;

/// A response's status and headers; the body follows on `body`.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|v| v.trim().parse().ok())
    }
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            s.push(if i <= c.len() {
                T[(n >> (18 - 6 * i)) as usize & 63] as char
            } else {
                '='
            });
        }
    }
    s
}

/// Percent-encode a path's bytes, keeping `/` and the characters URLs allow as they are.
pub fn encode_path(raw: &[u8]) -> String {
    let mut s = String::new();
    for &b in raw {
        if b.is_ascii_alphanumeric() || b"/-_.~!$&'()*+,;=:@".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

pub fn decode_percent(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' && i + 2 < s.len() {
            let hex = std::str::from_utf8(&s[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn month(m: &str) -> Option<u32> {
    let m = m.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|x| m.starts_with(x))
        .map(|i| i as u32 + 1)
}

/// `13-Nov-1997 10:04` (Apache index pages).
fn index_date(s: &str) -> Option<i64> {
    let mut it = s.split_whitespace();
    let d = it.next()?;
    let t = it.next()?;
    let mut p = d.split('-');
    let (day, mon, year) = (
        p.next()?.parse().ok()?,
        month(p.next()?)?,
        p.next()?.parse::<i64>().ok()?,
    );
    let year = if year < 100 { year + 1900 } else { year };
    let (h, m) = t.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.get(..2)?.parse().ok()?);
    Some(days_from_civil(year, mon, day) * 86400 + h * 3600 + m * 60)
}

/// `Thu, 13 Nov 1997 10:04:00 GMT` (Last-Modified).
fn http_date(s: &str) -> Option<i64> {
    let f: Vec<&str> = s.split([' ', ',']).filter(|x| !x.is_empty()).collect();
    let (day, mon, year, time) = (
        f.get(1)?.parse().ok()?,
        month(f.get(2)?)?,
        f.get(3)?.parse::<i64>().ok()?,
        f.get(4)?,
    );
    let t: Vec<i64> = time.split(':').filter_map(|x| x.parse().ok()).collect();
    Some(
        days_from_civil(year, mon, day) * 86400
            + t.first()? * 3600
            + t.get(1)? * 60
            + t.get(2).unwrap_or(&0),
    )
}

/// `1.2K`, `345`, `2M` after a link in an index page.
fn index_size(s: &str) -> Option<u64> {
    let t = s.trim();
    let (num, mult) = match t.chars().last()? {
        'K' | 'k' => (&t[..t.len() - 1], 1024.0),
        'M' | 'm' => (&t[..t.len() - 1], 1024.0 * 1024.0),
        'G' | 'g' => (&t[..t.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (t, 1.0),
    };
    let v: f64 = num.parse().ok()?;
    Some((v * mult) as u64)
}

/// The links an index page makes to things directly inside `dir` (its raw path, with `/` at the end).
pub fn parse_index(html: &[u8], dir: &[u8], cs: lc_core::Charset) -> Vec<Entry> {
    let lower: Vec<u8> = html.iter().map(u8::to_ascii_lowercase).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut at = 0;
    while let Some(i) = find(&lower[at..], b"<a ") {
        let start = at + i;
        let Some(end) = find(&lower[start..], b">").map(|e| start + e) else {
            break;
        };
        at = end + 1;
        let tag = &html[start..end];
        let Some(h) = find(&lower[start..end], b"href") else {
            continue;
        };
        let mut v = &tag[h + 4..];
        v = v.trim_ascii_start();
        let Some(rest) = v.strip_prefix(b"=") else {
            continue;
        };
        let rest = rest.trim_ascii_start();
        let href: &[u8] = match rest.first() {
            Some(&q @ (b'"' | b'\'')) => {
                let r = &rest[1..];
                &r[..r.iter().position(|&c| c == q).unwrap_or(r.len())]
            }
            _ => {
                &rest[..rest
                    .iter()
                    .position(|c| c.is_ascii_whitespace())
                    .unwrap_or(rest.len())]
            }
        };
        let mut href = href;
        if href.is_empty()
            || href.starts_with(b"?")
            || href.starts_with(b"#")
            || find(href, b":").is_some_and(|c| !href[..c].contains(&b'/'))
        {
            continue;
        }
        if href.starts_with(b"/") {
            match href.strip_prefix(dir) {
                Some(r) => href = r,
                None => continue,
            }
        }
        if let Some(r) = href.strip_prefix(b"./") {
            href = r;
        }
        let folder = href.ends_with(b"/");
        let bare = href.strip_suffix(b"/").unwrap_or(href);
        if bare.is_empty() || bare == b".." || bare == b"." || bare.contains(&b'/') {
            continue;
        }
        let bare = match bare.iter().position(|&c| c == b'?' || c == b'#') {
            Some(0) => continue,
            Some(p) => &bare[..p],
            None => bare,
        };
        let raw = decode_percent(bare);
        if !seen.insert(raw.clone()) {
            continue;
        }
        let mut e = Entry::new(
            part_name(&cs.decode(&raw)),
            if folder {
                EntryKind::Folder
            } else {
                EntryKind::File
            },
        );
        e.raw_name = raw;
        // What follows the link on its line: `13-Nov-1997 10:04  1.2K` in Apache's lists.
        let after = find(&lower[at..], b"</a>")
            .map(|c| at + c + 4)
            .unwrap_or(at);
        let line_end = html[after..]
            .iter()
            .position(|&c| c == b'\n' || c == b'<')
            .map_or(html.len(), |p| after + p);
        let tail = String::from_utf8_lossy(&html[after..line_end]).into_owned();
        let words: Vec<&str> = tail.split_whitespace().collect();
        if words.len() >= 2 {
            if let Some(t) = index_date(&format!("{} {}", words[0], words[1])) {
                e.modified = Some(Timestamp {
                    unix: t,
                    precision: TimePrecision::LocalUnknownZone,
                });
                if !folder {
                    e.size = words.get(2).and_then(|w| index_size(w));
                }
            }
        }
        out.push(e);
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A web server browsed through its index pages.
pub struct HttpSession {
    host: String,
    port: u16,
    auth: Option<String>,
    opts: Options,
    /// The `Server:` header, such as `Apache/1.3.6 (Unix)` or `WebSTAR/2.0`.
    pub server: String,
    closed: bool,
}

/// Check the server answers; `user` (when not empty) and `password` are sent as Basic auth.
pub fn connect(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    opts: &Options,
) -> Result<HttpSession> {
    let mut s = HttpSession {
        host: host.to_string(),
        port,
        auth: (!user.is_empty()).then(|| b64(format!("{user}:{password}").as_bytes())),
        opts: opts.clone(),
        server: String::new(),
        closed: false,
    };
    let r = s.request("HEAD", b"/", None)?;
    if r.status == 401 {
        return Err(Error::Auth(
            "the server wants a user name and password".into(),
        ));
    }
    s.server = r.header("server").unwrap_or("").to_string();
    Ok(s)
}

impl HttpSession {
    /// One request on its own connection.
    pub fn request(&self, method: &str, raw_path: &[u8], range: Option<u64>) -> Result<Response> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let mut s = dial(&self.host, self.port, &self.opts)?;
        let mut req = format!(
            "{method} {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: legcon\r\nAccept: */*\r\n",
            encode_path(raw_path),
            if self.port == PORT {
                self.host.clone()
            } else {
                format!("{}:{}", self.host, self.port)
            }
        );
        if let Some(a) = &self.auth {
            req.push_str(&format!("Authorization: Basic {a}\r\n"));
        }
        if let Some(n) = range {
            req.push_str(&format!("Range: bytes={n}-\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())?;
        read_response(s, method == "HEAD")
    }

    fn raw(&self, p: &RemotePath) -> Vec<u8> {
        let mut raw = Vec::new();
        for part in p.parts() {
            raw.push(b'/');
            raw.extend(
                self.opts
                    .charset
                    .encode(part)
                    .unwrap_or_else(|| part.as_bytes().to_vec()),
            );
        }
        if raw.is_empty() {
            raw.push(b'/');
        }
        raw
    }

    fn check(&self, r: &Response, what: &str) -> Result<()> {
        match r.status {
            200..=299 => Ok(()),
            401 => Err(Error::Auth(format!(
                "{what}: the server wants a user name and password"
            ))),
            403 => Err(Error::Denied(format!(
                "{what}: the server does not show this"
            ))),
            404 | 410 => Err(Error::NotFound(what.to_string())),
            n => Err(Error::Protocol(format!("{what}: the server answered {n}"))),
        }
    }
}

fn read_response(s: TcpStream, head: bool) -> Result<Response> {
    let mut r = BufReader::new(s);
    let mut line = Vec::new();
    r.read_until(b'\n', &mut line)?;
    let status_line = String::from_utf8_lossy(&line).trim().to_string();
    let mut parts = status_line.split_whitespace();
    let proto = parts.next().unwrap_or("");
    if !proto.starts_with("HTTP/") {
        // An HTTP/0.9 server answers with the body alone.
        let mut first = line.clone();
        let body: Box<dyn Read + Send> =
            Box::new(std::io::Cursor::new(std::mem::take(&mut first)).chain(r));
        return Ok(Response {
            status: 200,
            headers: Vec::new(),
            body,
        });
    }
    let status: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| Error::Protocol(format!("not an HTTP answer: {status_line}")))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let l = String::from_utf8_lossy(&line).trim_end().to_string();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut resp = Response {
        status,
        headers,
        body: Box::new(std::io::empty()),
    };
    if !head {
        resp.body = match resp.length() {
            Some(n) => Box::new(r.take(n)),
            None => Box::new(r),
        };
    }
    Ok(resp)
}

impl Connection for HttpSession {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}

impl FileSession for HttpSession {
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: true,
            ..FileCaps::default()
        }
    }

    fn home(&mut self) -> Result<RemotePath> {
        Ok(RemotePath::root(PathStyle::Unix))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        let mut raw = self.raw(dir);
        if !raw.ends_with(b"/") {
            raw.push(b'/');
        }
        let what = dir.to_string();
        let mut r = self.request("GET", &raw, None)?;
        if matches!(r.status, 301 | 302) {
            if let Some(loc) = r.header("location").map(str::to_string) {
                let path = loc.split_once("://").map_or(loc.as_str(), |(_, rest)| {
                    rest.find('/').map_or("/", |i| &rest[i..])
                });
                raw = decode_percent(path.as_bytes());
                r = self.request("GET", &raw, None)?;
            }
        }
        self.check(&r, &what)?;
        let mut html = Vec::new();
        r.body.take(4 << 20).read_to_end(&mut html)?;
        Ok(parse_index(&html, &raw, self.opts.charset))
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let what = path.to_string();
        let r = self.request("HEAD", &self.raw(path), None)?;
        let raw_name = self
            .opts
            .charset
            .encode(path.name())
            .unwrap_or_else(|| path.name().as_bytes().to_vec());
        if matches!(r.status, 301 | 302) {
            let mut e = Entry::new(path.name(), EntryKind::Folder);
            e.raw_name = raw_name;
            return Ok(e);
        }
        self.check(&r, &what)?;
        let mut e = Entry::new(path.name(), EntryKind::File);
        e.raw_name = raw_name;
        e.size = r.length();
        e.modified = r
            .header("last-modified")
            .and_then(http_date)
            .map(|t| Timestamp {
                unix: t,
                precision: TimePrecision::Exact,
            });
        Ok(e)
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        let r = self.request("GET", &self.raw(path), (at > 0).then_some(at))?;
        self.check(&r, &path.to_string())?;
        let mut body = r.body;
        if at > 0 && r.status != 206 {
            std::io::copy(&mut (&mut body).take(at), &mut std::io::sink())?;
        }
        Ok(body)
    }

    fn open_write(
        &mut self,
        _path: &RemotePath,
        _mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        Err(read_only())
    }

    fn mkdir(&mut self, _path: &RemotePath) -> Result<()> {
        Err(read_only())
    }

    fn remove(&mut self, _path: &RemotePath) -> Result<()> {
        Err(read_only())
    }

    fn rename(&mut self, _from: &RemotePath, _to: &RemotePath) -> Result<()> {
        Err(read_only())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apache_index_page() {
        let page = br#"<HTML><HEAD><TITLE>Index of /pub</TITLE></HEAD><BODY>
<H1>Index of /pub</H1>
<PRE><IMG SRC="/icons/blank.gif" ALT="     "> <A HREF="?N=D">Name</A>  <A HREF="?M=A">Last modified</A>
<HR>
<IMG SRC="/icons/back.gif" ALT="[DIR]"> <A HREF="/">Parent Directory</A>        13-Nov-1997 10:04      -
<IMG SRC="/icons/folder.gif" ALT="[DIR]"> <A HREF="gnu/">gnu/</A>                   12-Oct-1997 09:00      -
<IMG SRC="/icons/text.gif" ALT="[TXT]"> <A HREF="README">README</A>               13-Nov-1997 10:04     2k
<IMG SRC="/icons/text.gif" ALT="[TXT]"> <A HREF="caf%E9.txt">caf&eacute;.txt</A>   01-Jan-1998 00:00   345
<A HREF="http://www.apache.org/">Apache</A> <a href='/pub/x11/'>x11</a>
</PRE></BODY></HTML>"#;
        let e = parse_index(page, b"/pub/", lc_core::Charset::Latin1);
        let names: Vec<_> = e
            .iter()
            .map(|e| (e.name.as_str(), e.is_folder(), e.size))
            .collect();
        assert_eq!(
            names,
            [
                ("gnu", true, None),
                ("README", false, Some(2048)),
                ("caf\u{e9}.txt", false, Some(345)),
                ("x11", true, None)
            ]
        );
        assert_eq!(e[1].modified.unwrap().unix, 879_415_440);
    }

    #[test]
    fn dates_and_escapes() {
        assert_eq!(
            http_date("Thu, 13 Nov 1997 10:04:00 GMT"),
            Some(879_415_440)
        );
        assert_eq!(encode_path(b"/a b/caf\xe9"), "/a%20b/caf%E9");
        assert_eq!(decode_percent(b"a%20b%zz%"), b"a b%zz%");
        assert_eq!(b64(b"dani:pw"), "ZGFuaTpwdw==");
    }
}
