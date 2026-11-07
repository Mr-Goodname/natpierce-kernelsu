//! One local RFC6455 client with incremental nonblocking frame parsing.
use base64::{engine::general_purpose::STANDARD, Engine};
use sha1::{Digest, Sha1};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const LIMIT: usize = 1_048_576;
const HEADER_LIMIT: usize = 16_384;

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

// v1.06 occasionally writes GBK error text into text frames on Linux.
// Decode complete messages so a multibyte character split across fragments survives.
fn decode_text(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => encoding_rs::GBK.decode(error.as_bytes()).0.into_owned(),
    }
}

fn random(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::getrandom(bytes).map_err(|e| io::Error::other(e.to_string()))
}

pub struct Ws {
    socket: TcpStream,
    input: Vec<u8>,
    output: Vec<u8>,
    sent: usize,
    fragmented: Vec<u8>,
    fragment_opcode: Option<u8>,
    output_started: Option<Instant>,
}

impl Ws {
    pub fn connect(port: u16) -> io::Result<Self> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut socket = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.set_write_timeout(Some(Duration::from_secs(3)))?;
        socket.set_nodelay(true)?;
        let mut nonce = [0; 16];
        random(&mut nonce)?;
        let key = STANDARD.encode(nonce);
        let request = format!("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nOrigin: http://127.0.0.1:{port}\r\n\r\n");
        socket.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        let header_end = loop {
            if let Some(n) = response.windows(4).position(|w| w == b"\r\n\r\n") { break n + 4; }
            if response.len() > HEADER_LIMIT { return Err(invalid("WebSocket handshake header too large")); }
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf)?;
            if n == 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "core closed during WebSocket handshake")); }
            response.extend_from_slice(&buf[..n]);
        };
        if header_end > HEADER_LIMIT { return Err(invalid("WebSocket handshake header too large")); }
        let headers = std::str::from_utf8(&response[..header_end]).map_err(|_| invalid("invalid WebSocket HTTP header"))?;
        let mut lines = headers.split("\r\n");
        let status = lines.next().unwrap_or("");
        if status.split_ascii_whitespace().nth(1) != Some("101") { return Err(invalid("core rejected WebSocket upgrade")); }
        let mut accept = "";
        let mut upgrade = false;
        let mut connection = false;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("Sec-WebSocket-Accept") { accept = value.trim(); }
                if name.eq_ignore_ascii_case("Upgrade") { upgrade = value.trim().eq_ignore_ascii_case("websocket"); }
                if name.eq_ignore_ascii_case("Connection") {
                    connection = value.split(',').any(|s| s.trim().eq_ignore_ascii_case("upgrade"));
                }
            }
        }
        let expected = STANDARD.encode(Sha1::digest(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
        if accept != expected || !upgrade || !connection { return Err(invalid("invalid WebSocket upgrade response")); }
        let input = response[header_end..].to_vec();
        socket.set_read_timeout(None)?;
        socket.set_write_timeout(None)?;
        socket.set_nonblocking(true)?;
        Ok(Self { socket, input, output: Vec::new(), sent: 0, fragmented: Vec::new(), fragment_opcode: None, output_started: None })
    }

    pub fn send(&mut self, text: &str) -> io::Result<()> {
        self.queue_frame(1, text.as_bytes())?;
        self.flush()
    }

    fn queue_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        if payload.len() > LIMIT || self.output.len().saturating_sub(self.sent) + payload.len() + 14 > LIMIT + 14 {
            return Err(invalid("WebSocket output buffer limit exceeded"));
        }
        if self.sent > 0 {
            self.output.drain(..self.sent);
            self.sent = 0;
        }
        let mut mask = [0; 4];
        random(&mut mask)?;
        self.output.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => self.output.push(0x80 | n as u8),
            n if n <= u16::MAX as usize => {
                self.output.push(0x80 | 126);
                self.output.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                self.output.push(0x80 | 127);
                self.output.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        self.output.extend_from_slice(&mask);
        for (i, byte) in payload.iter().enumerate() { self.output.push(byte ^ mask[i % 4]); }
        if self.output_started.is_none() { self.output_started = Some(Instant::now()); }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        while self.sent < self.output.len() {
            match self.socket.write(&self.output[self.sent..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "core stopped reading control frames")),
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if self.output_started.map(|s| s.elapsed() > Duration::from_secs(5)).unwrap_or(false) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "WebSocket write stalled"));
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.output.clear();
        self.sent = 0;
        self.output_started = None;
        Ok(())
    }

    /// Read any available complete messages, retaining partial frames across calls.
    pub fn poll(&mut self) -> io::Result<Vec<String>> {
        self.flush()?;
        let mut messages = Vec::new();
        self.parse_frames(&mut messages)?;
        let mut budget = LIMIT + 14;
        while budget > 0 {
            let mut buf = [0; 8192];
            let take = buf.len().min(budget);
            match self.socket.read(&mut buf[..take]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "core closed control channel")),
                Ok(n) => {
                    budget -= n;
                    if self.input.len() + n > LIMIT + 14 { return Err(invalid("WebSocket input buffer limit exceeded")); }
                    self.input.extend_from_slice(&buf[..n]);
                    self.parse_frames(&mut messages)?;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.flush()?;
        Ok(messages)
    }

    fn parse_frames(&mut self, messages: &mut Vec<String>) -> io::Result<()> {
        let mut used = 0;
        while self.input.len() - used >= 2 {
            let data = &self.input[used..];
            let fin = data[0] & 0x80 != 0;
            let opcode = data[0] & 0x0f;
            if data[0] & 0x70 != 0 || data[1] & 0x80 != 0 {
                return Err(invalid("unsupported extension or masked server frame"));
            }
            let (length, head) = match data[1] & 0x7f {
                126 if data.len() < 4 => break,
                126 => {
                    let n = u16::from_be_bytes([data[2], data[3]]) as usize;
                    if n < 126 { return Err(invalid("noncanonical WebSocket length")); }
                    (n, 4)
                }
                127 if data.len() < 10 => break,
                127 => {
                    let n = u64::from_be_bytes(data[2..10].try_into().unwrap());
                    if n < 65536 || n > LIMIT as u64 { return Err(invalid("invalid WebSocket length")); }
                    (n as usize, 10)
                }
                n => (n as usize, 2),
            };
            if length > LIMIT { return Err(invalid("WebSocket frame too large")); }
            if opcode >= 8 && (!fin || length > 125) { return Err(invalid("invalid WebSocket control frame")); }
            if data.len() < head + length { break; }
            let payload = data[head..head + length].to_vec();
            used += head + length;
            match opcode {
                0 => {
                    let op = self.fragment_opcode.ok_or_else(|| invalid("unexpected continuation frame"))?;
                    if self.fragmented.len() + payload.len() > LIMIT { return Err(invalid("WebSocket message too large")); }
                    self.fragmented.extend_from_slice(&payload);
                    if fin {
                        if op == 1 {
                            messages.push(decode_text(std::mem::take(&mut self.fragmented)));
                        } else { self.fragmented.clear(); }
                        self.fragment_opcode = None;
                    }
                }
                1 | 2 => {
                    if self.fragment_opcode.is_some() { return Err(invalid("new data frame during fragmented message")); }
                    if fin {
                        if opcode == 1 { messages.push(decode_text(payload)); }
                    } else {
                        self.fragment_opcode = Some(opcode);
                        self.fragmented = payload;
                    }
                }
                8 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "core sent WebSocket close")),
                9 => self.queue_frame(10, &payload)?,
                10 => {}
                _ => return Err(invalid("unknown WebSocket opcode")),
            }
        }
        if used > 0 { self.input.drain(..used); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn ws_for_parser() -> (Ws, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = listener.accept().unwrap().0;
        socket.set_nonblocking(true).unwrap();
        (Ws { socket, input: vec![], output: vec![], sent: 0, fragmented: vec![], fragment_opcode: None, output_started: None }, peer)
    }

    #[test]
    fn partial_fragment_and_interleaved_ping() {
        let (mut ws, _peer) = ws_for_parser();
        let mut messages = vec![];
        ws.input.extend_from_slice(&[0x01, 3, b'a']);
        ws.parse_frames(&mut messages).unwrap();
        assert_eq!(ws.input.len(), 3);
        ws.input.extend_from_slice(&[b'b', b'c', 0x89, 1, b'p', 0x80, 2, b'd', b'e']);
        ws.parse_frames(&mut messages).unwrap();
        assert_eq!(messages, ["abcde"]);
        assert!(ws.input.is_empty());
        assert_eq!(ws.output[0], 0x8a);
        assert_eq!(ws.output[1], 0x81);
        assert_eq!(ws.output[6] ^ ws.output[2], b'p');
    }

    #[test]
    fn reject_bad_control_and_oversized_frames() {
        let (mut ws, _peer) = ws_for_parser();
        ws.input = vec![0x09, 0];
        assert!(ws.parse_frames(&mut vec![]).is_err());
        ws.input = vec![0x81, 127];
        ws.input.extend_from_slice(&((LIMIT as u64) + 1).to_be_bytes());
        assert!(ws.parse_frames(&mut vec![]).is_err());
        ws.input = vec![0x80, 0];
        assert!(ws.parse_frames(&mut vec![]).is_err());
    }

    #[test]
    fn decode_legacy_gbk_error() {
        assert_eq!(decode_text(vec![0xb3, 0xac, 0xca, 0xb1]), "超时");
        assert_eq!(decode_text("中文 UTF-8".as_bytes().to_vec()), "中文 UTF-8");
    }
}
