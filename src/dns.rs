//! Loopback DNS compatibility for the pure-Go Linux core on Android.
//! No system resolver files, netd settings, or global routes are modified.
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
use std::thread;
use std::time::{Duration, Instant};

fn add_ip(values: &mut Vec<IpAddr>, text: &str) {
    let text = text.trim().trim_start_matches('/');
    if let Ok(addr) = text.split('%').next().unwrap_or("").parse::<IpAddr>() {
        if !addr.is_loopback() && !addr.is_unspecified() && !addr.is_multicast() && !values.contains(&addr) {
            values.push(addr);
        }
    }
}

fn discover() -> Vec<IpAddr> {
    let mut values = Vec::new();
    for prop in ["net.dns1", "net.dns2", "net.dns3", "net.dns4"] {
        if let Ok(out) = Command::new("/system/bin/getprop").arg(prop).output() {
            add_ip(&mut values, &String::from_utf8_lossy(&out.stdout));
        }
    }
    if let Ok(out) = Command::new("/system/bin/dumpsys").arg("connectivity").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for part in text.split("DnsAddresses: [").skip(1) {
            if let Some((list, _)) = part.split_once(']') {
                for address in list.split(',') { add_ip(&mut values, address); }
            }
        }
    }
    for fallback in ["223.5.5.5", "119.29.29.29"] { add_ip(&mut values, fallback); }
    values.truncate(6);
    values
}

fn valid_response(query: &[u8], response: &[u8]) -> bool {
    response.len() >= 12 && response[..2] == query[..2] && response[2] & 0x80 != 0
}

fn tcp_query(query: &[u8], upstream: SocketAddr) -> io::Result<Vec<u8>> {
    let mut connection = TcpStream::connect_timeout(&upstream, Duration::from_millis(900))?;
    connection.set_read_timeout(Some(Duration::from_millis(900)))?;
    connection.set_write_timeout(Some(Duration::from_millis(900)))?;
    connection.write_all(&(query.len() as u16).to_be_bytes())?;
    connection.write_all(query)?;
    let mut len = [0; 2];
    connection.read_exact(&mut len)?;
    let n = u16::from_be_bytes(len) as usize;
    if n < 12 { return Err(io::Error::new(io::ErrorKind::InvalidData, "short DNS response")); }
    let mut response = vec![0; n];
    connection.read_exact(&mut response)?;
    Ok(response)
}

fn resolve(query: &[u8], upstreams: &[IpAddr]) -> Vec<u8> {
    for &ip in upstreams {
        let bind = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let result = (|| -> io::Result<Vec<u8>> {
            let connection = UdpSocket::bind(bind)?;
            let upstream = SocketAddr::new(ip, 53);
            connection.connect(upstream)?;
            connection.set_read_timeout(Some(Duration::from_millis(900)))?;
            connection.set_write_timeout(Some(Duration::from_millis(900)))?;
            connection.send(query)?;
            let mut buf = [0; 65535];
            let n = connection.recv(&mut buf)?;
            let response = buf[..n].to_vec();
            if !valid_response(query, &response) { return Err(io::Error::new(io::ErrorKind::InvalidData, "mismatched DNS response")); }
            if response[2] & 0x02 != 0 { return tcp_query(query, upstream); }
            Ok(response)
        })();
        if let Ok(response) = result {
            if valid_response(query, &response) && response[3] & 0x0f != 2 && response[3] & 0x0f != 5 { return response; }
        }
    }
    let mut failure = query.to_vec();
    failure[2] |= 0x80;
    failure[3] = (failure[3] & 0xf0) | 0x82;
    failure[6..12].fill(0);
    failure
}

/// Starts a compatibility DNS listener that lives only as long as the daemon process.
/// An occupied port is reported, never killed or claimed by force.
pub fn start() -> io::Result<()> {
    let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 53))?;
    listener.set_read_timeout(Some(Duration::from_millis(500)))?;
    let upstreams = Arc::new(Mutex::new(discover()));
    let active = Arc::new(AtomicUsize::new(0));
    thread::Builder::new().name("np-dns".into()).spawn(move || {
        let mut refreshed = Instant::now();
        loop {
            if refreshed.elapsed() >= Duration::from_secs(60) {
                if let Ok(mut list) = upstreams.lock() { *list = discover(); }
                refreshed = Instant::now();
            }
            let mut buf = [0; 4096];
            let (n, peer) = match listener.recv_from(&mut buf) {
                Ok(result) => result,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => continue,
                Err(_) => break,
            };
            if n < 12 || buf[2] & 0x80 != 0 || u16::from_be_bytes([buf[4], buf[5]]) == 0 { continue; }
            if active.load(Ordering::Relaxed) >= 16 { continue; }
            let socket = match listener.try_clone() { Ok(s) => s, Err(_) => continue };
            let addresses = match upstreams.lock() { Ok(list) => list.clone(), Err(_) => continue };
            let query = buf[..n].to_vec();
            let pending = Arc::clone(&active);
            active.fetch_add(1, Ordering::Relaxed);
            if thread::Builder::new().name("np-dns-query".into()).spawn(move || {
                let answer = resolve(&query, &addresses);
                let _ = socket.send_to(&answer, peer);
                pending.fetch_sub(1, Ordering::Relaxed);
            }).is_err() {
                active.fetch_sub(1, Ordering::Relaxed);
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dns_response_requires_matching_transaction() {
        let query = [0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        let mut response = query;
        assert!(!valid_response(&query, &response));
        response[2] |= 0x80;
        assert!(valid_response(&query, &response));
        response[1] ^= 1;
        assert!(!valid_response(&query, &response));
    }
}
