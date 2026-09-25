// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! ClamAV through a running `clamd`, off unless `threat.clamd.address` is set.
//!
//! Each attachment is streamed with `zINSTREAM` (4-byte big-endian chunk
//! lengths, a zero-length chunk to finish) over a Unix socket or TCP, with a
//! 5 s budget per attachment. Attachments go to the configured daemon only;
//! nothing leaves the machine unless the operator points `tcp:` elsewhere.
//!
//! `FOUND` is `malware_detected` (+100, malware-grade). A clamd error is
//! recorded as a skipped analyzer, or makes the verdict `unavailable` when
//! `threat.clamd.required` is true.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use super::attachments::fingerprint;
use super::config::ClamdAddress;
use super::{Analyzer, Signal, ThreatInput};
use crate::ingress::MAX_ATTACHMENT_BYTES;

pub const MALWARE_DETECTED: u32 = 100;
pub const TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK: usize = 64 * 1024;
/// Longest reply read back; real replies are one short line.
const MAX_REPLY: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClamdReply {
    Clean,
    Found(String),
}

/// Parse a `zINSTREAM` reply (`stream: OK`, `stream: <name> FOUND`).
pub fn parse_reply(reply: &str) -> Result<ClamdReply, String> {
    let reply = reply.trim_end_matches(['\0', '\n', '\r']).trim();
    let body = reply
        .strip_prefix("stream:")
        .map(str::trim)
        .ok_or_else(|| format!("clamd replied `{reply}`"))?;
    if body == "OK" {
        return Ok(ClamdReply::Clean);
    }
    if let Some(name) = body.strip_suffix("FOUND") {
        return Ok(ClamdReply::Found(name.trim().to_string()));
    }
    Err(format!("clamd replied `{reply}`"))
}

trait Timed: Read + Write {
    fn set_timeouts(&self, remaining: Duration) -> std::io::Result<()>;
}

impl Timed for TcpStream {
    fn set_timeouts(&self, remaining: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(remaining))?;
        self.set_write_timeout(Some(remaining))
    }
}

#[cfg(unix)]
impl Timed for std::os::unix::net::UnixStream {
    fn set_timeouts(&self, remaining: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(remaining))?;
        self.set_write_timeout(Some(remaining))
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| format!("clamd did not answer within {TIMEOUT:?}"))
}

fn instream<S: Timed>(stream: &mut S, bytes: &[u8], deadline: Instant) -> Result<String, String> {
    let io = |e: std::io::Error| format!("clamd connection failed: {e}");
    stream.set_timeouts(remaining(deadline)?).map_err(io)?;
    stream.write_all(b"zINSTREAM\0").map_err(io)?;
    for chunk in bytes.chunks(CHUNK) {
        stream.set_timeouts(remaining(deadline)?).map_err(io)?;
        stream
            .write_all(&(chunk.len() as u32).to_be_bytes())
            .map_err(io)?;
        stream.write_all(chunk).map_err(io)?;
    }
    stream.write_all(&0u32.to_be_bytes()).map_err(io)?;
    stream.flush().map_err(io)?;

    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    while reply.len() < MAX_REPLY && !reply.contains(&0) {
        stream.set_timeouts(remaining(deadline)?).map_err(io)?;
        let n = stream.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        reply.extend_from_slice(&buf[..n]);
    }
    if reply.is_empty() {
        return Err("clamd closed the connection without a reply".into());
    }
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

/// Stream `bytes` to clamd and parse its answer.
pub fn scan(address: &ClamdAddress, bytes: &[u8]) -> Result<ClamdReply, String> {
    let deadline = Instant::now() + TIMEOUT;
    let reply = match address {
        ClamdAddress::Unix(path) => {
            #[cfg(unix)]
            {
                let mut stream = std::os::unix::net::UnixStream::connect(path)
                    .map_err(|e| format!("clamd at unix:{} unreachable: {e}", path.display()))?;
                instream(&mut stream, bytes, deadline)?
            }
            #[cfg(not(unix))]
            {
                return Err(format!(
                    "unix:{} needs a Unix platform; use tcp:host:port",
                    path.display()
                ));
            }
        }
        ClamdAddress::Tcp(hostport) => {
            let addrs: Vec<_> = hostport
                .to_socket_addrs()
                .map_err(|e| format!("clamd at tcp:{hostport} unresolvable: {e}"))?
                .collect();
            let mut last_err = format!("clamd at tcp:{hostport} resolved to no address");
            let mut connected = None;
            for addr in addrs {
                match TcpStream::connect_timeout(&addr, remaining(deadline)?) {
                    Ok(stream) => {
                        connected = Some(stream);
                        break;
                    }
                    Err(e) => last_err = format!("clamd at tcp:{hostport} unreachable: {e}"),
                }
            }
            let mut stream = connected.ok_or(last_err)?;
            instream(&mut stream, bytes, deadline)?
        }
    };
    parse_reply(&reply)
}

pub struct ClamdAnalyzer {
    pub address: ClamdAddress,
    pub required: bool,
}

impl Analyzer for ClamdAnalyzer {
    fn name(&self) -> &'static str {
        "clamd"
    }

    fn required(&self) -> bool {
        self.required
    }

    /// A detection wins over errors on other attachments: the message is
    /// malware either way.
    fn analyze(&self, input: &ThreatInput) -> Result<Vec<Signal>, String> {
        let mut signals = Vec::new();
        let mut errors = Vec::new();
        for att in &input.attachments {
            let fp = fingerprint(att);
            let Some(bytes) = att.bytes.as_deref() else {
                errors.push(format!("{fp}: attachment bytes were not fetched"));
                continue;
            };
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                errors.push(format!(
                    "{fp}: {} bytes exceeds the {MAX_ATTACHMENT_BYTES}-byte scan limit",
                    bytes.len()
                ));
                continue;
            }
            match scan(&self.address, bytes) {
                Ok(ClamdReply::Clean) => {}
                Ok(ClamdReply::Found(name)) => signals.push(
                    Signal::new(
                        "malware_detected",
                        MALWARE_DETECTED,
                        format!("clamd {name}: {fp}"),
                    )
                    .malware(),
                ),
                Err(e) => errors.push(format!("{fp}: {e}")),
            }
        }
        if signals.is_empty() && !errors.is_empty() {
            return Err(errors.join("; "));
        }
        Ok(signals)
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::ThreatConfig;
    use super::super::{Level, evaluate};
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// The fake daemon decides the verdict, so the fixture bytes are inert
    /// (no EICAR test string in the tree for a real scanner to trip on).
    const SAMPLE: &[u8] = b"inert fixture bytes standing in for a flagged file";

    /// A one-connection fake clamd: reads the INSTREAM framing, reports the
    /// reassembled bytes, answers `reply`.
    fn fake_clamd_on(listener: TcpListener, reply: &'static str) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut cmd = [0u8; 10];
            sock.read_exact(&mut cmd).unwrap();
            assert_eq!(&cmd, b"zINSTREAM\0");
            let mut data = Vec::new();
            loop {
                let mut len = [0u8; 4];
                sock.read_exact(&mut len).unwrap();
                let len = u32::from_be_bytes(len) as usize;
                if len == 0 {
                    break;
                }
                let mut chunk = vec![0u8; len];
                sock.read_exact(&mut chunk).unwrap();
                data.extend_from_slice(&chunk);
            }
            sock.write_all(reply.as_bytes()).unwrap();
            tx.send(data).unwrap();
        });
        rx
    }

    fn fake_clamd(reply: &'static str) -> (ClamdAddress, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ClamdAddress::Tcp(listener.local_addr().unwrap().to_string());
        (addr, fake_clamd_on(listener, reply))
    }

    fn with_attachment(bytes: &[u8]) -> ThreatInput {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let raw = format!(
            "From: a@sender.example\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=b\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
             --b\r\nContent-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{b64}\r\n--b--\r\n"
        );
        let mut input = ThreatInput::from_raw(raw.as_bytes(), "me@example.org").unwrap();
        input.ledger = Ok(Default::default());
        input
    }

    #[test]
    fn replies_parse() {
        assert_eq!(parse_reply("stream: OK\0"), Ok(ClamdReply::Clean));
        assert_eq!(
            parse_reply("stream: Eicar-Test-Signature FOUND\0"),
            Ok(ClamdReply::Found("Eicar-Test-Signature".into()))
        );
        assert!(parse_reply("INSTREAM size limit exceeded. ERROR\0").is_err());
    }

    #[test]
    fn instream_sends_the_bytes_and_reads_found() {
        let (addr, got) = fake_clamd("stream: Eicar-Test-Signature FOUND\0");
        let big: Vec<u8> = SAMPLE
            .iter()
            .copied()
            .cycle()
            .take(CHUNK * 2 + 17)
            .collect();
        assert_eq!(
            scan(&addr, &big).unwrap(),
            ClamdReply::Found("Eicar-Test-Signature".into())
        );
        assert_eq!(got.recv().unwrap(), big, "chunks reassemble byte-exact");
    }

    #[test]
    fn found_is_malware_grade_and_ok_is_clean() {
        let (addr, _) = fake_clamd("stream: Eicar-Test-Signature FOUND\0");
        let analyzer = ClamdAnalyzer {
            address: addr,
            required: false,
        };
        let signals = analyzer.analyze(&with_attachment(SAMPLE)).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].code, "malware_detected");
        assert_eq!(signals[0].weight, 100);
        assert!(signals[0].malware);
        assert!(
            signals[0]
                .evidence
                .starts_with("clamd Eicar-Test-Signature: ext=.pdf sha256=")
        );
        assert!(!signals[0].evidence.contains("invoice"), "no filenames");

        let (addr, _) = fake_clamd("stream: OK\0");
        let analyzer = ClamdAnalyzer {
            address: addr,
            required: false,
        };
        assert!(
            analyzer
                .analyze(&with_attachment(b"%PDF-1.4"))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_transport_works() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clamd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            while !seen.ends_with(&[0, 0, 0, 0]) {
                let n = sock.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
            sock.write_all(b"stream: OK\0").unwrap();
        });
        assert_eq!(
            scan(&ClamdAddress::Unix(path), b"hello").unwrap(),
            ClamdReply::Clean
        );
    }

    fn dead_address() -> ClamdAddress {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        ClamdAddress::Tcp(addr)
    }

    #[test]
    fn required_mode_makes_clamd_errors_unavailable() {
        let input = with_attachment(b"%PDF-1.4");
        let config = ThreatConfig::default();

        let required: Vec<Box<dyn Analyzer>> = vec![Box::new(ClamdAnalyzer {
            address: dead_address(),
            required: true,
        })];
        let v = evaluate(&input, &required, &config);
        assert_eq!(v.level, Level::Unavailable);
        assert!(v.analyzers_skipped[0].reason.contains("unreachable"));

        let optional: Vec<Box<dyn Analyzer>> = vec![Box::new(ClamdAnalyzer {
            address: dead_address(),
            required: false,
        })];
        let v = evaluate(&input, &optional, &config);
        assert_eq!(v.level, Level::Clean);
        assert_eq!(v.analyzers_skipped[0].name, "clamd");
    }

    #[test]
    fn silent_clamd_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ClamdAddress::Tcp(listener.local_addr().unwrap().to_string());
        let hold = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            std::thread::sleep(TIMEOUT + Duration::from_secs(1));
            drop(sock);
        });
        let started = Instant::now();
        let err = scan(&addr, b"x").unwrap_err();
        assert!(
            started.elapsed() < TIMEOUT + Duration::from_millis(900),
            "{err}"
        );
        hold.join().unwrap();
    }
}
