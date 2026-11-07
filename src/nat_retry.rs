//! Bounded retransmission of the official core's initial paired NAT probes.
//! Captured payloads are never logged. An observed UDP port is eligible only
//! when its namespace socket inode is also a file descriptor of the live core.
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read},
    mem,
    net::Ipv4Addr,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{atomic::{AtomicU32, AtomicU64, Ordering}, Arc},
    thread,
    time::{Duration, Instant},
};

const RELAYS: [[u8; 4]; 2] = [[1, 14, 72, 144], [59, 110, 17, 170]];
const PORT: u16 = 33272;
const MAX_JOBS: usize = 16;
const MAX_FRAMES: usize = 256;
const MAX_PROC: u64 = 2 * 1024 * 1024;
const REPEATS: u8 = 8;
const INTERVAL: Duration = Duration::from_millis(50);
const REFRESH: Duration = Duration::from_millis(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity { pid: u32, start_time: u64 }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Tuple { src: [u8; 4], dst: [u8; 4], src_port: u16, dst_port: u16 }

struct Datagram<'a> { tuple: Tuple, header_len: usize, packet: &'a [u8], payload: &'a [u8] }

fn datagram(frame: &[u8]) -> Option<Datagram<'_>> {
    if frame.len() < 28 || frame[0] >> 4 != 4 || frame[9] != 17 { return None; }
    let header_len = usize::from(frame[0] & 15) * 4;
    if !(20..=60).contains(&header_len) || frame.len() < header_len + 8 { return None; }
    let total = usize::from(u16::from_be_bytes([frame[2], frame[3]]));
    if total < header_len + 8 || total > frame.len() { return None; }
    // Reject every fragment, including a first fragment with MF set.
    if u16::from_be_bytes([frame[6], frame[7]]) & 0x3fff != 0 { return None; }
    let udp = &frame[header_len..total];
    if usize::from(u16::from_be_bytes([udp[4], udp[5]])) != udp.len() { return None; }
    Some(Datagram {
        tuple: Tuple {
            src: frame[12..16].try_into().ok()?, dst: frame[16..20].try_into().ok()?,
            src_port: u16::from_be_bytes([udp[0], udp[1]]),
            dst_port: u16::from_be_bytes([udp[2], udp[3]]),
        },
        header_len, packet: &frame[..total], payload: &udp[8..],
    })
}

fn is_probe(d: &Datagram<'_>) -> bool {
    d.tuple.dst_port == PORT && d.tuple.src_port != 0 && d.tuple.src != [0; 4]
        && RELAYS.contains(&d.tuple.dst) && d.payload == b"$ZJOK"
}

fn is_reply(d: &Datagram<'_>, tuple: Tuple) -> bool {
    if d.tuple.src != tuple.dst || d.tuple.dst != tuple.src
        || d.tuple.src_port != tuple.dst_port || d.tuple.dst_port != tuple.src_port { return false; }
    let Ok(text) = std::str::from_utf8(d.payload) else { return false; };
    let Some((address, port)) = text.split_once(':') else { return false; };
    address.parse::<Ipv4Addr>().is_ok() && port.parse::<u16>().is_ok_and(|p| p != 0)
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(2) {
        sum += u32::from(chunk[0]) << 8;
        if chunk.len() == 2 { sum += u32::from(chunk[1]); }
    }
    while sum >> 16 != 0 { sum = (sum & 0xffff) + (sum >> 16); }
    !(sum as u16)
}

fn replay_packet(d: &Datagram<'_>) -> Vec<u8> {
    let mut packet = d.packet.to_vec();
    // A zero IP ID lets Linux generate an ID. Retain TTL, DSCP, options and DF.
    packet[4..6].fill(0);
    packet[10..12].fill(0);
    let h = d.header_len;
    packet[h + 6..h + 8].fill(0);
    let mut pseudo = Vec::with_capacity(12 + packet.len() - h);
    pseudo.extend_from_slice(&d.tuple.src);
    pseudo.extend_from_slice(&d.tuple.dst);
    pseudo.extend_from_slice(&[0, 17]);
    pseudo.extend_from_slice(&((packet.len() - h) as u16).to_be_bytes());
    pseudo.extend_from_slice(&packet[h..]);
    let udp_sum = checksum(&pseudo);
    // A computed zero must be transmitted as 0xffff, not "checksum disabled".
    packet[h + 6..h + 8].copy_from_slice(&if udp_sum == 0 { 0xffff } else { udp_sum }.to_be_bytes());
    let ip_sum = checksum(&packet[..h]);
    packet[10..12].copy_from_slice(&ip_sum.to_be_bytes());
    packet
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalAddress { Any, V4([u8; 4]) }

#[derive(Clone, Copy, Debug)]
struct Binding { address: LocalAddress, port: u16, inode: u64 }

fn local_address(text: &str, ipv6: bool) -> Option<LocalAddress> {
    if !ipv6 {
        if text.len() != 8 { return None; }
        let bytes = u32::from_str_radix(text, 16).ok()?.to_le_bytes();
        return Some(if bytes == [0; 4] { LocalAddress::Any } else { LocalAddress::V4(bytes) });
    }
    if text.len() != 32 { return None; }
    let mut bytes = [0u8; 16];
    for (i, word) in text.as_bytes().chunks_exact(8).enumerate() {
        let word = std::str::from_utf8(word).ok()?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&u32::from_str_radix(word, 16).ok()?.to_le_bytes());
    }
    if bytes == [0; 16] { return Some(LocalAddress::Any); }
    if bytes[..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255] {
        let v4: [u8; 4] = bytes[12..].try_into().ok()?;
        return Some(if v4 == [0; 4] { LocalAddress::Any } else { LocalAddress::V4(v4) });
    }
    None // A true IPv6 binding cannot own one of these captured IPv4 probes.
}

fn parse_bindings(table: &str, ipv6: bool) -> io::Result<Vec<Binding>> {
    let mut bindings = Vec::new();
    for line in table.lines().skip(1).filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 { return Err(io::Error::other("incomplete UDP socket table")); }
        let Some((address, port)) = fields[1].split_once(':') else { return Err(io::Error::other("invalid UDP local address")); };
        let port = u16::from_str_radix(port, 16).map_err(|_| io::Error::other("invalid UDP port"))?;
        let inode = fields[9].parse().map_err(|_| io::Error::other("invalid UDP socket inode"))?;
        if let Some(address) = local_address(address, ipv6) { bindings.push(Binding { address, port, inode }); }
    }
    Ok(bindings)
}

fn socket_inode(link: &str) -> Option<u64> { link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok() }

fn start_time(stat: &str) -> Option<u64> {
    // comm can contain spaces and parentheses. Field 3 starts after its last ')'.
    stat.get(stat.rfind(')')? + 1..)?.split_whitespace().nth(19)?.parse().ok()
}

fn proc_text(path: &str) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?.take(MAX_PROC).read_to_string(&mut text)?;
    if text.len() as u64 == MAX_PROC { return Err(io::Error::other("socket table exceeded bound")); }
    Ok(text)
}

struct Ownership { identity: Identity, fd_inodes: HashSet<u64>, bindings: Vec<Binding> }

impl Ownership {
    fn refresh(pid: u32) -> io::Result<Self> {
        if pid <= 1 { return Err(io::Error::other("core is not running")); }
        let stat_path = format!("/proc/{pid}/stat");
        let initial = start_time(&proc_text(&stat_path)?).ok_or_else(|| io::Error::other("invalid core identity"))?;
        let mut fd_inodes = HashSet::new();
        for (index, entry) in fs::read_dir(format!("/proc/{pid}/fd"))?.enumerate() {
            if index >= 4096 { return Err(io::Error::other("core descriptor scan exceeded bound")); }
            // Descriptors may disappear while the Go core closes a probe socket.
            let entry = entry?;
            if let Ok(link) = fs::read_link(entry.path()) {
                if let Some(inode) = link.to_str().and_then(socket_inode) { fd_inodes.insert(inode); }
            }
        }
        let mut bindings = parse_bindings(&proc_text(&format!("/proc/{pid}/net/udp"))?, false)?;
        match proc_text(&format!("/proc/{pid}/net/udp6")) {
            Ok(table) => bindings.extend(parse_bindings(&table, true)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {},
            Err(e) => return Err(e),
        }
        let final_time = start_time(&proc_text(&stat_path)?).ok_or_else(|| io::Error::other("invalid core identity"))?;
        if initial != final_time { return Err(io::Error::other("core identity changed")); }
        Ok(Self { identity: Identity { pid, start_time: initial }, fd_inodes, bindings })
    }

    fn inode(&self, tuple: Tuple) -> Option<u64> {
        let mut candidate = None;
        for binding in &self.bindings {
            if binding.port != tuple.src_port || !matches!(binding.address, LocalAddress::Any)
                && binding.address != LocalAddress::V4(tuple.src) { continue; }
            // Fail closed when multiple sockets share the source port. This also
            // avoids confusing an IPv6-only wildcard with another app's IPv4 fd.
            if candidate.is_some_and(|inode| inode != binding.inode) { return None; }
            candidate = Some(binding.inode);
        }
        candidate.filter(|inode| self.fd_inodes.contains(inode))
    }
}

struct Job {
    tuple: Tuple, identity: Identity, inode: u64, packet: Vec<u8>,
    started: Instant, attempts: u8, cancelled: bool,
}

impl Job {
    fn owned(&self, ownership: Option<&Ownership>) -> bool {
        ownership.is_some_and(|o| o.identity == self.identity && o.inode(self.tuple) == Some(self.inode))
    }
    fn due(&self, now: Instant) -> bool {
        !self.cancelled && self.attempts < REPEATS
            && now.duration_since(self.started) >= INTERVAL * u32::from(self.attempts + 1)
    }
}

fn validate_jobs(jobs: &mut Vec<Job>, ownership: Option<&Ownership>) {
    if ownership.is_some() {
        jobs.retain(|job| job.owned(ownership));
    } else {
        // A transient /proc failure must stop sends without forgetting dedupe
        // state: an already queued replay may otherwise start a second burst.
        for job in jobs { job.cancelled = true; }
    }
}

fn socket(family: i32, kind: i32, protocol: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(family, kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, protocol) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn filter_capture(fd: &OwnedFd) -> io::Result<()> {
    // Classic socket BPF runs before queuing to this AF_PACKET socket. Only
    // relay IPv4 UDP reaches userspace, so tunnel data cannot bury NAT replies.
    // For SOCK_DGRAM the network header begins at offset zero.
    let instructions: [(u16, u8, u8, u32); 23] = [
        (0x30, 0, 0, 9),       // LD B [IPv4 protocol]
        (0x15, 0, 20, 17),     // UDP or reject
        (0x28, 0, 0, 6),       // LD H [fragment flags/offset]
        (0x45, 18, 0, 0x3fff), // Reject MF or a fragment offset
        (0x30, 0, 0, 0),       // LD B [version/IHL]
        (0x54, 0, 0, 0xf0),    // AND version mask
        (0x15, 0, 15, 0x40),   // IPv4 or reject
        (0x20, 0, 0, 12),      // LD W [source IP]
        (0x15, 5, 0, 0x010e4890),
        (0x15, 4, 0, 0x3b6e11aa),
        (0x20, 0, 0, 16),      // LD W [destination IP]
        (0x15, 2, 0, 0x010e4890),
        (0x15, 1, 0, 0x3b6e11aa),
        (0x06, 0, 0, 0),       // RET reject
        (0xb1, 0, 0, 0),       // LDX MSH [0], IPv4 header length
        (0x48, 0, 0, 0),       // LD H [X+0], UDP source port
        (0x15, 2, 0, PORT as u32),
        (0x48, 0, 0, 2),       // LD H [X+2], UDP destination port
        (0x15, 0, 3, PORT as u32),
        (0x48, 0, 0, 4),       // LD H [X+4], UDP length
        (0x25, 1, 0, 40),      // NAT replies fit in 40 bytes; exclude relay data
        (0x06, 0, 0, 0xffff),  // RET accept
        (0x06, 0, 0, 0),       // RET reject
    ];
    let mut filters: Vec<libc::sock_filter> = instructions.into_iter().map(|(code, jt, jf, k)|
        libc::sock_filter { code, jt, jf, k }).collect();
    let program = libc::sock_fprog { len: filters.len() as u16, filter: filters.as_mut_ptr() };
    if unsafe { libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_ATTACH_FILTER,
        (&program as *const libc::sock_fprog).cast(), mem::size_of_val(&program) as libc::socklen_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn send(raw: &OwnedFd, job: &Job) -> bool {
    let mut address: libc::sockaddr_in = unsafe { mem::zeroed() };
    address.sin_family = libc::AF_INET as _;
    address.sin_addr.s_addr = u32::from_ne_bytes(job.tuple.dst);
    (unsafe {
        libc::sendto(raw.as_raw_fd(), job.packet.as_ptr().cast(), job.packet.len(), 0,
            (&address as *const libc::sockaddr_in).cast(), mem::size_of_val(&address) as libc::socklen_t)
    }) == job.packet.len() as isize
}

fn run(capture: OwnedFd, raw: OwnedFd, core_pid: Arc<AtomicU32>, count: Arc<AtomicU64>) {
    let mut ownership = None;
    let mut refreshed = Instant::now().checked_sub(REFRESH).unwrap_or_else(Instant::now);
    let mut jobs = Vec::<Job>::new();
    let mut frame = [0u8; 4096];
    loop {
        let cycle_started = Instant::now();
        let mut pid = core_pid.load(Ordering::Acquire);
        let now = Instant::now();
        let identity_changed = ownership.as_ref().is_some_and(|o: &Ownership| o.identity.pid != pid);
        if identity_changed || pid <= 1 { ownership = None; jobs.clear(); }
        let active = jobs.iter().any(|job| !job.cancelled && job.attempts < REPEATS);
        if active && now.duration_since(refreshed) >= REFRESH {
            ownership = Ownership::refresh(pid).ok();
            refreshed = now;
            validate_jobs(&mut jobs, ownership.as_ref());
        }
        let mut refreshed_for_candidate = false;
        let mut poll = libc::pollfd { fd: capture.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // Idle operation blocks in poll and performs no periodic /proc scan.
        // A new probe wakes immediately; only an active burst polls every 20ms.
        let ready = unsafe { libc::poll(&mut poll, 1, if active { 20 } else { 1000 }) };
        let current_pid = core_pid.load(Ordering::Acquire);
        if current_pid != pid { pid = current_pid; ownership = None; jobs.clear(); }
        if ready > 0 && poll.revents & libc::POLLIN != 0 {
            for _ in 0..MAX_FRAMES {
                let mut sender: libc::sockaddr_ll = unsafe { mem::zeroed() };
                let mut sender_len = mem::size_of_val(&sender) as libc::socklen_t;
                let n = unsafe { libc::recvfrom(capture.as_raw_fd(), frame.as_mut_ptr().cast(), frame.len(), 0,
                    (&mut sender as *mut libc::sockaddr_ll).cast(), &mut sender_len) };
                if n < 0 { break; }
                if sender_len < mem::size_of::<libc::sockaddr_ll>() as _ { continue; }
                let Some(d) = datagram(&frame[..n as usize]) else { continue; };
                if sender.sll_pkttype != 4 { // PACKET_OUTGOING
                    for job in &mut jobs { if is_reply(&d, job.tuple) { job.cancelled = true; } }
                    continue;
                }
                if !is_probe(&d) || pid <= 1 { continue; }
                if !refreshed_for_candidate {
                    // An idle socket can be created after our last scan. Refresh
                    // before testing tombstones in case this port was recycled.
                    ownership = Ownership::refresh(pid).ok();
                    refreshed = Instant::now();
                    refreshed_for_candidate = true;
                    validate_jobs(&mut jobs, ownership.as_ref());
                }
                if jobs.iter().any(|job| job.tuple == d.tuple) { continue; } // Includes our own replay frames.
                let inode = ownership.as_ref().and_then(|o| o.inode(d.tuple));
                let Some(inode) = inode else { continue; };
                let Some(owner) = ownership.as_ref() else { continue; };
                if jobs.len() >= MAX_JOBS || core_pid.load(Ordering::Acquire) != pid { continue; }
                jobs.push(Job { tuple: d.tuple, identity: owner.identity, inode, packet: replay_packet(&d),
                    started: Instant::now(), attempts: 0, cancelled: false });
            }
        }
        let now = Instant::now();
        if jobs.iter().any(|job| job.due(now)) && now.duration_since(refreshed) >= Duration::from_millis(20) {
            ownership = Ownership::refresh(pid).ok();
            refreshed = Instant::now();
            validate_jobs(&mut jobs, ownership.as_ref());
        }
        for job in &mut jobs {
            if core_pid.load(Ordering::Acquire) != job.identity.pid { break; }
            if job.due(now) && job.owned(ownership.as_ref()) {
                job.attempts += 1; // Failed send attempts are bounded too.
                if send(&raw, job) { count.fetch_add(1, Ordering::Relaxed); }
            }
        }
        // Completed/cancelled jobs remain as tombstones until their socket closes;
        // captured replays and queued original packets cannot reset their timer.
        if jobs.iter().any(|job| !job.cancelled && job.attempts < REPEATS) {
            let remaining = Duration::from_millis(20).saturating_sub(cycle_started.elapsed());
            if !remaining.is_zero() { thread::sleep(remaining); }
        }
    }
}

/// Start one daemon-lifetime worker. The caller publishes only its verified live
/// core child PID (zero while stopped). The result counts successful raw replays.
pub fn start(core_pid: Arc<AtomicU32>) -> io::Result<Arc<AtomicU64>> {
    let capture = socket(libc::AF_PACKET, libc::SOCK_DGRAM, u16::to_be(3) as i32)?;
    filter_capture(&capture)?;
    let raw = socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_RAW)?;
    let enabled = 1i32;
    if unsafe { libc::setsockopt(raw.as_raw_fd(), libc::IPPROTO_IP, libc::IP_HDRINCL,
        (&enabled as *const i32).cast(), mem::size_of_val(&enabled) as libc::socklen_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let count = Arc::new(AtomicU64::new(0));
    let worker_count = count.clone();
    thread::Builder::new().name("np-nat-retry".into()).spawn(move || run(capture, raw, core_pid, worker_count))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> Vec<u8> {
        let mut p = vec![0; 33];
        p[0] = 0x45; p[2..4].copy_from_slice(&33u16.to_be_bytes()); p[8] = 128; p[9] = 17;
        p[12..16].copy_from_slice(&[192, 168, 5, 56]); p[16..20].copy_from_slice(&RELAYS[0]);
        p[20..22].copy_from_slice(&31001u16.to_be_bytes()); p[22..24].copy_from_slice(&PORT.to_be_bytes());
        p[24..26].copy_from_slice(&13u16.to_be_bytes()); p[28..].copy_from_slice(b"$ZJOK"); p
    }

    #[test]
    fn parser_rejects_fragments_truncation_and_unrelated_udp() {
        let p = packet(); assert!(is_probe(&datagram(&p).unwrap()));
        for cut in 0..p.len() { assert!(datagram(&p[..cut]).is_none()); }
        for (index, value) in [(0, 0x65), (0, 0x44), (6, 0x20), (7, 1), (9, 6), (25, 12)] {
            let mut bad = p.clone(); bad[index] = value; assert!(datagram(&bad).is_none());
        }
        let mut unrelated = p.clone(); unrelated[28] = b'X'; assert!(!is_probe(&datagram(&unrelated).unwrap()));
        unrelated = p.clone(); unrelated[16] = 8; assert!(!is_probe(&datagram(&unrelated).unwrap()));
        unrelated = p; unrelated[23] ^= 1; assert!(!is_probe(&datagram(&unrelated).unwrap()));
    }

    #[test]
    fn replay_preserves_tuple_and_has_valid_odd_length_checksums() {
        assert_eq!(checksum(&[0, 1, 2]), 0xfdfe);
        let p = packet(); let d = datagram(&p).unwrap(); let replay = replay_packet(&d);
        assert_eq!(datagram(&replay).unwrap().tuple, d.tuple); assert_eq!(replay[8], 128);
        assert_eq!(&replay[4..6], &[0, 0]); assert_eq!(checksum(&replay[..20]), 0);
        let mut pseudo = Vec::from(d.tuple.src); pseudo.extend_from_slice(&d.tuple.dst);
        pseudo.extend_from_slice(&[0, 17, 0, 13]); pseudo.extend_from_slice(&replay[20..]);
        assert_eq!(checksum(&pseudo), 0); assert_ne!(&replay[26..28], &[0, 0]);
    }

    #[test]
    fn namespace_socket_table_requires_fd_intersection_and_rejects_ambiguous_port() {
        let table = "sl local_address rem_address st tx_queue rx_queue tr tm retr uid timeout inode\n0: 00000000:7919 00000000:0000 07 0:0 0:0 0 0 0 111 2\n1: 3805A8C0:791A 00000000:0000 07 0:0 0:0 0 0 0 222 2\n";
        let bindings = parse_bindings(table, false).unwrap();
        let mut o = Ownership { identity: Identity { pid: 42, start_time: 100 }, fd_inodes: HashSet::from([222]), bindings };
        let tuple = datagram(&packet()).unwrap().tuple;
        assert_eq!(o.inode(tuple), None); o.fd_inodes.insert(111); assert_eq!(o.inode(tuple), Some(111));
        o.bindings.push(Binding { address: LocalAddress::V4(tuple.src), port: tuple.src_port, inode: 999 });
        assert_eq!(o.inode(tuple), None);
        assert_eq!(local_address("0000000000000000FFFF00003805A8C0", true), Some(LocalAddress::V4(tuple.src)));
        assert_eq!(local_address("00000000000000000000000000000000", true), Some(LocalAddress::Any));
        assert_eq!(socket_inode("socket:[1234]"), Some(1234)); assert_eq!(socket_inode("pipe:[1234]"), None);
        assert!(parse_bindings("header\nmalformed", false).is_err());
    }

    #[test]
    fn duplicate_jobs_stay_bounded_and_stop_for_inode_or_generation_change() {
        let p = packet(); let d = datagram(&p).unwrap(); let now = Instant::now();
        let identity = Identity { pid: 42, start_time: 100 };
        let mut o = Ownership { identity, fd_inodes: HashSet::from([111]), bindings: vec![Binding { address: LocalAddress::Any, port: d.tuple.src_port, inode: 111 }] };
        let mut job = Job { tuple: d.tuple, identity, inode: 111, packet: replay_packet(&d), started: now, attempts: 0, cancelled: false };
        assert!(!job.due(now)); assert!(job.due(now + INTERVAL)); assert!(job.owned(Some(&o)));
        job.attempts = REPEATS; assert!(!job.due(now + Duration::from_secs(60)));
        o.identity.start_time += 1; assert!(!job.owned(Some(&o))); o.identity = identity;
        o.bindings[0].inode = 222; o.fd_inodes.insert(222); assert!(!job.owned(Some(&o)));
        let stat = format!("42 (core with ) spaces) S {} 98765", vec!["0"; 18].join(" "));
        assert_eq!(start_time(&stat), Some(98765));
    }
}
