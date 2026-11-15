//! The Telnet network virtual terminal, sans I/O: bytes in, data and replies out.
//!
//! Options follow RFC 1143's rule of never answering a confirmation, so two sides can't loop:
//! a request we made is remembered, and the matching answer is taken silently. legcon agrees to
//! BINARY, SGA and ECHO from the server, and offers TTYPE, NAWS, SGA and BINARY itself; every
//! other option is refused.

pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const SE: u8 = 240;

pub const BINARY: u8 = 0;
pub const ECHO: u8 = 1;
pub const SGA: u8 = 3;
pub const TTYPE: u8 = 24;
pub const NAWS: u8 = 31;

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;
/// A subnegotiation longer than this is junk; it is dropped rather than buffered forever.
const SB_MAX: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    /// After a CR in data: a following NUL is dropped (NVT's bare carriage return).
    Cr,
    Iac,
    Verb(u8),
    Sb,
    SbIac,
}

/// What one call to [`Nvt::receive`] produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Received {
    /// Bytes for the terminal.
    pub data: Vec<u8>,
    /// Bytes to send back to the server (negotiation answers).
    pub reply: Vec<u8>,
}

/// One Telnet connection's option state.
#[derive(Debug)]
pub struct Nvt {
    term: String,
    cols: u16,
    rows: u16,
    state: State,
    sb: Vec<u8>,
    /// Options we perform (we said WILL and they said DO).
    local: [bool; 256],
    /// Options the server performs (it said WILL and we said DO).
    remote: [bool; 256],
    /// WILLs we sent and still wait to have answered.
    asked_local: [bool; 256],
    /// DOs we sent and still wait to have answered.
    asked_remote: [bool; 256],
}

fn cmd(verb: u8, opt: u8) -> [u8; 3] {
    [IAC, verb, opt]
}

impl Nvt {
    pub fn new(term: &str, cols: u16, rows: u16) -> Nvt {
        Nvt {
            term: term.to_ascii_uppercase(),
            cols,
            rows,
            state: State::Data,
            sb: Vec::new(),
            local: [false; 256],
            remote: [false; 256],
            asked_local: [false; 256],
            asked_remote: [false; 256],
        }
    }

    /// What a client says first: it will give its terminal type and size, and wants SGA.
    pub fn greeting(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for opt in [TTYPE, NAWS] {
            self.asked_local[opt as usize] = true;
            out.extend(cmd(WILL, opt));
        }
        self.asked_remote[SGA as usize] = true;
        out.extend(cmd(DO, SGA));
        out
    }

    /// Whether the server echoes what we type (passwords are typed with this on and nothing shows).
    pub fn server_echoes(&self) -> bool {
        self.remote[ECHO as usize]
    }

    fn binary_out(&self) -> bool {
        self.local[BINARY as usize]
    }

    fn accept_remote(opt: u8) -> bool {
        matches!(opt, BINARY | ECHO | SGA)
    }

    fn accept_local(opt: u8) -> bool {
        matches!(opt, BINARY | SGA | TTYPE | NAWS)
    }

    fn naws(&self) -> Vec<u8> {
        let mut out = vec![IAC, SB, NAWS];
        for v in [self.cols, self.rows] {
            for b in v.to_be_bytes() {
                // A 255 in the size is doubled, like any IAC in a subnegotiation.
                out.push(b);
                if b == IAC {
                    out.push(IAC);
                }
            }
        }
        out.extend([IAC, SE]);
        out
    }

    /// The window changed size; the bytes to send (none until the server agreed to NAWS).
    pub fn resize(&mut self, cols: u16, rows: u16) -> Vec<u8> {
        self.cols = cols;
        self.rows = rows;
        if self.local[NAWS as usize] {
            self.naws()
        } else {
            Vec::new()
        }
    }

    fn verb(&mut self, verb: u8, opt: u8, reply: &mut Vec<u8>) {
        let o = opt as usize;
        match verb {
            WILL => {
                if self.asked_remote[o] {
                    self.asked_remote[o] = false;
                    self.remote[o] = true;
                } else if !self.remote[o] {
                    if Self::accept_remote(opt) {
                        self.remote[o] = true;
                        reply.extend(cmd(DO, opt));
                    } else {
                        reply.extend(cmd(DONT, opt));
                    }
                }
            }
            WONT => {
                if self.asked_remote[o] {
                    self.asked_remote[o] = false;
                } else if self.remote[o] {
                    reply.extend(cmd(DONT, opt));
                }
                self.remote[o] = false;
            }
            DO => {
                if self.asked_local[o] {
                    self.asked_local[o] = false;
                    self.local[o] = true;
                } else if !self.local[o] {
                    if Self::accept_local(opt) {
                        self.local[o] = true;
                        reply.extend(cmd(WILL, opt));
                    } else {
                        reply.extend(cmd(WONT, opt));
                    }
                }
                if opt == NAWS && self.local[o] {
                    reply.extend(self.naws());
                }
            }
            DONT => {
                if self.asked_local[o] {
                    self.asked_local[o] = false;
                } else if self.local[o] {
                    reply.extend(cmd(WONT, opt));
                }
                self.local[o] = false;
            }
            _ => {}
        }
    }

    fn subnegotiation(&mut self, reply: &mut Vec<u8>) {
        if self.sb.len() >= 2
            && self.sb[0] == TTYPE
            && self.sb[1] == TTYPE_SEND
            && self.local[TTYPE as usize]
        {
            reply.extend([IAC, SB, TTYPE, TTYPE_IS]);
            reply.extend(self.term.bytes());
            reply.extend([IAC, SE]);
        }
        self.sb.clear();
    }

    /// Feed bytes from the server.
    pub fn receive(&mut self, input: &[u8]) -> Received {
        let mut out = Received::default();
        for &b in input {
            self.state = match self.state {
                State::Data | State::Cr => match b {
                    IAC => State::Iac,
                    0 if self.state == State::Cr && !self.remote[BINARY as usize] => State::Data,
                    b'\r' => {
                        out.data.push(b);
                        State::Cr
                    }
                    _ => {
                        out.data.push(b);
                        State::Data
                    }
                },
                State::Iac => match b {
                    IAC => {
                        out.data.push(IAC);
                        State::Data
                    }
                    WILL | WONT | DO | DONT => State::Verb(b),
                    SB => {
                        self.sb.clear();
                        State::Sb
                    }
                    // NOP, GA, DM, BRK and the rest carry nothing for a terminal.
                    _ => State::Data,
                },
                State::Verb(v) => {
                    self.verb(v, b, &mut out.reply);
                    State::Data
                }
                State::Sb => match b {
                    IAC => State::SbIac,
                    _ => {
                        if self.sb.len() < SB_MAX {
                            self.sb.push(b);
                        }
                        State::Sb
                    }
                },
                State::SbIac => match b {
                    SE => {
                        self.subnegotiation(&mut out.reply);
                        State::Data
                    }
                    IAC => {
                        if self.sb.len() < SB_MAX {
                            self.sb.push(IAC);
                        }
                        State::Sb
                    }
                    _ => State::Sb,
                },
            };
        }
        out
    }

    /// Keystrokes for the wire: IAC doubled; a CR becomes CR LF, and stays a lone CR in binary
    /// (RFC 856: no CR stuffing there, so a NUL after it would reach the program as a key).
    pub fn encode(&self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 2);
        for &b in input {
            match b {
                IAC => out.extend([IAC, IAC]),
                b'\r' if self.binary_out() => out.push(b'\r'),
                b'\r' => out.extend(*b"\r\n"),
                _ => out.push(b),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_passes_and_iac_iac_is_one_byte() {
        let mut n = Nvt::new("vt100", 80, 24);
        let r = n.receive(&[b'h', b'i', IAC, IAC, b'\r', 0, b'x']);
        assert_eq!(r.data, [b'h', b'i', IAC, b'\r', b'x']);
        assert!(r.reply.is_empty());
    }

    #[test]
    fn our_requests_are_answered_silently() {
        let mut n = Nvt::new("vt100", 80, 24);
        let hello = n.greeting();
        assert_eq!(hello, [IAC, WILL, TTYPE, IAC, WILL, NAWS, IAC, DO, SGA]);
        // The server agrees to everything: no WILL / DO goes back, only the window size.
        let r = n.receive(&[IAC, DO, TTYPE, IAC, DO, NAWS, IAC, WILL, SGA]);
        assert_eq!(r.reply, [IAC, SB, NAWS, 0, 80, 0, 24, IAC, SE]);
        // A second DO NAWS (no state change) only repeats the size, never a WILL.
        let r = n.receive(&[IAC, DO, NAWS]);
        assert_eq!(r.reply, [IAC, SB, NAWS, 0, 80, 0, 24, IAC, SE]);
    }

    #[test]
    fn server_echo_accepted_unknown_options_refused() {
        let mut n = Nvt::new("vt100", 80, 24);
        let r = n.receive(&[IAC, WILL, ECHO, IAC, DO, 39, IAC, WILL, 34]);
        assert_eq!(r.reply, [IAC, DO, ECHO, IAC, WONT, 39, IAC, DONT, 34]);
        assert!(n.server_echoes());
        let r = n.receive(&[IAC, WONT, ECHO]);
        assert_eq!(r.reply, [IAC, DONT, ECHO]);
        assert!(!n.server_echoes());
    }

    #[test]
    fn terminal_type_and_resize() {
        let mut n = Nvt::new("vt100", 80, 24);
        n.greeting();
        n.receive(&[IAC, DO, TTYPE]);
        let r = n.receive(&[IAC, SB, TTYPE, TTYPE_SEND, IAC, SE]);
        let mut want = vec![IAC, SB, TTYPE, TTYPE_IS];
        want.extend(b"VT100");
        want.extend([IAC, SE]);
        assert_eq!(r.reply, want);
        assert!(
            n.resize(132, 50).is_empty(),
            "no NAWS until the server agrees"
        );
        n.receive(&[IAC, DO, NAWS]);
        assert_eq!(
            n.resize(255, 50),
            [IAC, SB, NAWS, 0, 255, 255, 0, 50, IAC, SE]
        );
    }

    #[test]
    fn encode_escapes_iac_and_newlines() {
        let n = Nvt::new("vt100", 80, 24);
        assert_eq!(
            n.encode(&[b'l', b's', b'\r', IAC]),
            [b'l', b's', b'\r', b'\n', IAC, IAC]
        );
        let mut n = Nvt::new("vt100", 80, 24);
        n.receive(&[IAC, DO, BINARY]);
        assert!(n.binary_out());
        assert_eq!(n.encode(b"ls\r"), b"ls\r");
    }

    #[test]
    fn options_split_across_reads() {
        let mut n = Nvt::new("vt100", 80, 24);
        assert!(n.receive(&[IAC]).reply.is_empty());
        assert!(n.receive(&[WILL]).reply.is_empty());
        assert_eq!(n.receive(&[ECHO, b'a']).reply, [IAC, DO, ECHO]);
    }
}
