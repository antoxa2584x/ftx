//! Transfer engine shared by the CLI and the GUI.
//!
//! Files are split into 4 MiB chunks hashed with BLAKE3. When a file already
//! exists locally (e.g. half-downloaded by a torrent client), both sides hash
//! it and only the chunks that differ are transferred.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

pub const CHUNK: u64 = 4 << 20;
pub const PROTO: u32 = 3;
/// Outgoing connections a sharing PC keeps open to a waiting receiver.
const DIAL_CONNS: usize = 8;
pub const DEFAULT_PORT: u16 = 9000;
const DONE_FILE: &str = ".ftx-done";
const MAX_RETRIES: u32 = 30;
/// Names torrent clients give unfinished files; adopted and renamed when found.
const TORRENT_SUFFIXES: &[&str] = &[".!qB", ".!ut", ".!bt", ".part", ".incomplete"];

/// Where progress/status messages go (stdout for the CLI, a log panel in the GUI).
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

// ---------- protocol ----------
// Every message is a u32 little-endian length + JSON. A connection starts
// with the `Auth` handshake, then the receiver sends `Req`s and the host
// answers with `Resp`s. `Stream` replies are followed by raw bytes: 32-byte
// hashes (Hashes) or hash+chunk pairs (Get).

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum Auth {
    Hello { proto: u32, nonce: String },
    Proof { proof: String },
    Denied { msg: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum Req {
    List,
    Hashes { path: String },
    Get { path: String, chunks: Vec<u64> },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum Resp {
    Err { msg: String },
    List { files: Vec<Entry> },
    Stream { size: u64 },
}

/// Frame size limits: before login, for requests, and for replies (a file list can be big).
const MAX_AUTH_FRAME: usize = 4 << 10;
const MAX_REQ_FRAME: usize = 64 << 20;
const MAX_RESP_FRAME: usize = 1 << 30;

#[derive(Serialize, Deserialize, Clone)]
pub struct Entry {
    pub path: String,
    pub size: u64,
}

fn send<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let b = serde_json::to_vec(msg)?;
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(&b)?;
    w.flush()
}

fn recv<T: DeserializeOwned>(r: &mut impl Read, max: usize) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut b = vec![0u8; len];
    r.read_exact(&mut b)?;
    Ok(serde_json::from_slice(&b)?)
}

/// `n` (at most 64) random hex digits, seeded from the OS via `RandomState`.
pub fn random_hex(n: usize) -> String {
    use std::hash::{BuildHasher, RandomState};
    let mut h = blake3::Hasher::new();
    for _ in 0..4 {
        h.update(&RandomState::new().hash_one(0u8).to_le_bytes());
    }
    h.update(format!("{:?}{}", std::time::SystemTime::now(), std::process::id()).as_bytes());
    h.finalize().to_hex()[..n].to_string()
}

/// Which end of the TCP connection we are; the one that dialed proves itself
/// first, so a stranger connecting to an open port learns nothing.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Dialer,
    Accepter,
}

fn auth_mac(token: &str, role: Role, dialer_nonce: &str, accepter_nonce: &str) -> blake3::Hash {
    let key = blake3::derive_key("ftx 2026-11 login v3", token.as_bytes());
    let role = if role == Role::Dialer { "dialer" } else { "accepter" };
    blake3::keyed_hash(&key, format!("{role}\n{dialer_nonce}\n{accepter_nonce}").as_bytes())
}

/// Check that both sides know the password without sending it.
fn handshake(r: &mut impl Read, w: &mut impl Write, token: &str, role: Role) -> io::Result<()> {
    let closed = |e: io::Error| match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => {
            io::Error::other("the other side closed the connection (wrong password or a different ftx version?)")
        }
        _ => e,
    };
    let mine = random_hex(32);
    send(w, &Auth::Hello { proto: PROTO, nonce: mine.clone() })?;
    let theirs = match recv(r, MAX_AUTH_FRAME).map_err(closed)? {
        Auth::Hello { proto, nonce } if proto == PROTO => nonce,
        Auth::Denied { msg } => return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg)),
        _ => {
            let msg = "the other PC runs a different version of ftx";
            let _ = send(w, &Auth::Denied { msg: msg.into() });
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
        }
    };
    let (dn, an) = if role == Role::Dialer { (&mine, &theirs) } else { (&theirs, &mine) };
    let peer = if role == Role::Dialer { Role::Accepter } else { Role::Dialer };
    let proof = Auth::Proof { proof: auth_mac(token, role, dn, an).to_hex().to_string() };
    if role == Role::Dialer {
        send(w, &proof)?;
    }
    match recv(r, MAX_AUTH_FRAME).map_err(closed)? {
        // blake3::Hash compares in constant time.
        Auth::Proof { proof } if blake3::Hash::from_hex(&proof).is_ok_and(|h| h == auth_mac(token, peer, dn, an)) => {}
        Auth::Denied { msg } => return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg)),
        _ => {
            let _ = send(w, &Auth::Denied { msg: "wrong password".into() });
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "wrong password"));
        }
    }
    if role == Role::Accepter {
        send(w, &proof)?;
    }
    Ok(())
}

fn n_chunks(size: u64) -> u64 {
    size.div_ceil(CHUNK)
}

fn chunk_len(size: u64, i: u64) -> usize {
    (size - i * CHUNK).min(CHUNK) as usize
}

/// Hash `file` chunk by chunk as if it were `size` bytes long. Chunks the file
/// doesn't fully contain are reported as None.
fn hash_chunks(
    file: &mut File,
    size: u64,
    mut on_chunk: impl FnMut(Option<[u8; 32]>, usize) -> io::Result<()>,
) -> io::Result<()> {
    let have = file.metadata()?.len();
    let mut buf = vec![0u8; CHUNK as usize];
    for i in 0..n_chunks(size) {
        let len = chunk_len(size, i);
        if i * CHUNK + len as u64 > have {
            on_chunk(None, len)?;
            continue;
        }
        file.read_exact(&mut buf[..len])?;
        on_chunk(Some(*blake3::hash(&buf[..len]).as_bytes()), len)?;
    }
    Ok(())
}

/// Join a '/'-separated relative path onto `root`, refusing anything that could escape it.
fn safe_join(root: &Path, rel: &str) -> io::Result<PathBuf> {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        // On Windows these would act as separators or drive/stream markers.
        let bad_char = cfg!(windows) && part.contains(['\\', ':']);
        if part.is_empty() || part == "." || part == ".." || bad_char {
            return Err(io::Error::other(format!("invalid path: {rel}")));
        }
        p.push(part);
    }
    Ok(p)
}

pub fn human(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

pub fn with_port(host: &str) -> String {
    let host = host.trim();
    if host.contains(':') { host.to_string() } else { format!("{host}:{DEFAULT_PORT}") }
}

pub fn top_level(path: &str) -> &str {
    path.split('/').next().unwrap()
}

/// (top-level name, total size, file count), sorted by name.
pub fn summarize(files: &[Entry]) -> Vec<(String, u64, usize)> {
    let mut groups: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
    for e in files {
        let g = groups.entry(top_level(&e.path)).or_default();
        g.0 += e.size;
        g.1 += 1;
    }
    groups.into_iter().map(|(k, (s, n))| (k.to_string(), s, n)).collect()
}

// ---------- host ----------

/// The host's address as seen from the internet once UPnP has been tried:
/// Ok("ip:port") if the router opened the port, Err(reason) if not, None while trying.
pub type PublicAddr = Arc<Mutex<Option<Result<String, String>>>>;

const UPNP_LEASE_SECS: u32 = 3600;

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64),
        IpAddr::V6(_) => false,
    }
}

/// Ask the router to forward `port` to this PC (UPnP, the way torrent clients
/// do it), keep the mapping alive while sharing, and remove it on stop.
fn upnp_keep_open(port: u16, stop: Arc<AtomicBool>, public: PublicAddr, log: Log) {
    use igd_next::{PortMappingProtocol::TCP, SearchOptions};
    let setup = || -> Result<(igd_next::Gateway, SocketAddr, IpAddr), String> {
        let mut opts = SearchOptions::default();
        opts.timeout = Some(Duration::from_secs(5));
        // Search from the adapter that leads to the internet; with virtual adapters
        // (Hyper-V, WSL, VPN) present, Windows may otherwise send it to the wrong one.
        if let Some(ip) = UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("8.8.8.8:80").and(s.local_addr()))
            .ok()
            .map(|a| a.ip())
        {
            opts.bind_addr = SocketAddr::new(ip, 0);
        }
        let gw = igd_next::search_gateway(opts).map_err(|e| format!("router did not answer UPnP ({e})"))?;
        let local_ip = UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect(gw.addr).and(s.local_addr()))
            .map_err(|e| e.to_string())?
            .ip();
        let local = SocketAddr::new(local_ip, port);
        let ext = gw.get_external_ip().map_err(|e| format!("router did not report its address ({e})"))?;
        // Some routers only accept permanent mappings.
        gw.add_port(TCP, port, local, UPNP_LEASE_SECS, "ftx file sharing")
            .or_else(|_| gw.add_port(TCP, port, local, 0, "ftx file sharing"))
            .map_err(|e| format!("router refused to open port {port} ({e})"))?;
        Ok((gw, local, ext))
    };
    let (gw, local, ext) = match setup() {
        Ok(x) => x,
        Err(e) => {
            log(format!("could not open the port automatically: {e}"));
            *public.lock().unwrap() = Some(Err(e));
            return;
        }
    };
    if is_private(ext) {
        let e = format!("the router's own address {ext} is not public (the internet provider shares one address between customers)");
        log(format!("port opened on the router, but {e}"));
        *public.lock().unwrap() = Some(Err(e));
    } else {
        log(format!("router opened port {port} automatically; internet address is {ext}:{port}"));
        *public.lock().unwrap() = Some(Ok(format!("{ext}:{port}")));
    }
    let mut since = 0;
    while !stop.load(Relaxed) {
        thread::sleep(Duration::from_secs(1));
        since += 1;
        if since >= UPNP_LEASE_SECS / 2 {
            since = 0;
            let _ = gw.add_port(TCP, port, local, UPNP_LEASE_SECS, "ftx file sharing");
        }
    }
    let _ = gw.remove_port(TCP, port);
}

/// Share `dir` until `stop` is set. Receivers can connect to `bind`; if `dial`
/// is given, this PC also keeps connecting out to a receiver waiting there
/// (for when this PC can't be reached from the internet).
pub fn serve(
    dir: &Path,
    bind: &str,
    token: &str,
    log: Log,
    stop: Arc<AtomicBool>,
    public: PublicAddr,
    dial: Option<String>,
) -> io::Result<()> {
    if token.is_empty() {
        return Err(io::Error::other("password must not be empty"));
    }
    let root = Arc::new(dir.canonicalize()?);
    let token = Arc::new(token.to_string());
    let listener = TcpListener::bind(bind)?;
    listener.set_nonblocking(true)?;
    let conns = Conns::default();
    // However serve() ends, stop the helper threads and drop every connection.
    let _cleanup = Cleanup { stop: stop.clone(), conns: conns.clone() };
    {
        let (port, stop, public, log) = (listener.local_addr()?.port(), stop.clone(), public.clone(), log.clone());
        thread::spawn(move || upnp_keep_open(port, stop, public, log));
    }
    let files = list_files(&root, &log);
    let total: u64 = files.iter().map(|e| e.size).sum();
    log(format!("sharing {} ({} files, {}) on {bind}", dir.display(), files.len(), human(total)));

    if let Some(addr) = dial {
        log(format!("connecting out to the receiver at {addr}..."));
        let last_err: Arc<Mutex<String>> = Arc::default();
        for _ in 0..DIAL_CONNS {
            let (addr, root, token, log, stop, conns, last_err) =
                (addr.clone(), root.clone(), token.clone(), log.clone(), stop.clone(), conns.clone(), last_err.clone());
            thread::spawn(move || {
                // All threads usually fail the same way; log each problem once.
                let report = |msg: String| {
                    let mut last = last_err.lock().unwrap();
                    if *last != msg {
                        log(msg.clone());
                        *last = msg;
                    }
                };
                let pause = || {
                    for _ in 0..50 {
                        if stop.load(Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                };
                while !stop.load(Relaxed) {
                    let sock = match dial_tcp(&addr) {
                        Ok(s) => s,
                        Err(e) => {
                            report(format!("cannot reach the receiver at {addr} ({e}); will keep trying"));
                            pause();
                            continue;
                        }
                    };
                    let _tracked = conns.track(&sock);
                    match handle(sock, &addr, &root, &token, &log, Role::Dialer) {
                        Ok(()) => last_err.lock().unwrap().clear(),
                        Err(_) if stop.load(Relaxed) => {}
                        // An unused connection timed out in the receiver's queue: just offer a fresh one.
                        Err(e) if is_timeout(&e) => {}
                        Err(e) => {
                            report(format!("{addr}: {e}"));
                            pause();
                        }
                    }
                }
            });
        }
    }
    while !stop.load(Relaxed) {
        let (sock, peer) = match listener.accept() {
            Ok(x) => x,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e),
        };
        sock.set_nonblocking(false)?;
        let tracked = conns.track(&sock);
        let (root, token, log) = (root.clone(), token.clone(), log.clone());
        thread::spawn(move || {
            let _tracked = tracked;
            if let Err(e) = handle(sock, &peer.to_string(), &root, &token, &log, Role::Accepter) {
                log(format!("{peer}: disconnected ({e})"));
            }
        });
    }
    log("stopped sharing".into());
    Ok(())
}

fn is_timeout(e: &io::Error) -> bool {
    // Read timeouts are TimedOut on Windows and WouldBlock on Unix.
    matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

/// Open connections, so they can all be closed when sharing stops.
#[derive(Clone, Default)]
struct Conns(Arc<Mutex<(u64, HashMap<u64, TcpStream>)>>);

impl Conns {
    /// Remember `sock` until the returned guard is dropped.
    fn track(&self, sock: &TcpStream) -> Tracked {
        let mut m = self.0.lock().unwrap();
        m.0 += 1;
        let id = m.0;
        if let Ok(s) = sock.try_clone() {
            m.1.insert(id, s);
        }
        Tracked(self.clone(), id)
    }

    fn close_all(&self) {
        for (_, s) in self.0.lock().unwrap().1.drain() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

struct Tracked(Conns, u64);

impl Drop for Tracked {
    fn drop(&mut self) {
        (self.0).0.lock().unwrap().1.remove(&self.1);
    }
}

struct Cleanup {
    stop: Arc<AtomicBool>,
    conns: Conns,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        self.conns.close_all();
    }
}

fn list_files(root: &Path, log: &Log) -> Vec<Entry> {
    let mut v = Vec::new();
    for e in walkdir::WalkDir::new(root).sort_by_file_name() {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                log(format!("warning: {err}"));
                continue;
            }
        };
        if !e.file_type().is_file() {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let rel = e.path().strip_prefix(root).unwrap();
        let path = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        v.push(Entry { path, size: meta.len() });
    }
    v
}

fn open_shared(root: &Path, rel: &str) -> io::Result<(File, u64)> {
    let f = File::open(safe_join(root, rel)?)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a file"));
    }
    Ok((f, meta.len()))
}

fn handle(sock: TcpStream, peer: &str, root: &Path, token: &str, log: &Log, role: Role) -> io::Result<()> {
    // Strangers get a short time to log in. A connection we dialed may sit
    // unused in the receiver's queue for a while; after 10 minutes we replace
    // it, which also keeps routers from silently dropping it.
    let login_wait = if role == Role::Accepter { 30 } else { 600 };
    sock.set_read_timeout(Some(Duration::from_secs(login_wait)))?;
    let mut r = BufReader::new(sock.try_clone()?);
    let mut w = BufWriter::with_capacity(1 << 20, sock);
    if let Err(e) = handshake(&mut r, &mut w, token, role) {
        // (Dialing threads report their own errors, once.)
        if e.kind() == io::ErrorKind::PermissionDenied && role == Role::Accepter {
            log(format!("{peer}: rejected ({e})"));
        }
        return Err(e);
    }
    // The receiver may spend a long time hashing its own copy between requests.
    r.get_ref().set_read_timeout(Some(Duration::from_secs(3600)))?;
    log(format!("{peer}: connected"));
    loop {
        let req = match recv::<Req>(&mut r, MAX_REQ_FRAME) {
            Ok(q) => q,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        match req {
            Req::List => send(&mut w, &Resp::List { files: list_files(root, log) })?,
            Req::Hashes { path } => {
                let (mut f, size) = match open_shared(root, &path) {
                    Ok(x) => x,
                    Err(e) => {
                        send(&mut w, &Resp::Err { msg: format!("{path}: {e}") })?;
                        continue;
                    }
                };
                log(format!("{peer}: checking {path} ({})", human(size)));
                send(&mut w, &Resp::Stream { size })?;
                hash_chunks(&mut f, size, |h, _| {
                    let h = h.ok_or_else(|| io::Error::other("file shrank while hashing"))?;
                    w.write_all(&h)?;
                    w.flush()
                })?;
            }
            Req::Get { path, chunks } => {
                let (mut f, size) = match open_shared(root, &path) {
                    Ok(x) => x,
                    Err(e) => {
                        send(&mut w, &Resp::Err { msg: format!("{path}: {e}") })?;
                        continue;
                    }
                };
                log(format!("{peer}: sending {path} ({} of {} chunks)", chunks.len(), n_chunks(size)));
                send(&mut w, &Resp::Stream { size })?;
                let mut buf = vec![0u8; CHUNK as usize];
                for i in chunks {
                    if i >= n_chunks(size) {
                        return Err(io::Error::other("bad chunk index"));
                    }
                    let len = chunk_len(size, i);
                    f.seek(SeekFrom::Start(i * CHUNK))?;
                    f.read_exact(&mut buf[..len])?;
                    w.write_all(blake3::hash(&buf[..len]).as_bytes())?;
                    w.write_all(&buf[..len])?;
                }
                w.flush()?;
            }
        }
    }
}

// ---------- client ----------

struct Conn {
    r: BufReader<TcpStream>,
    w: BufWriter<TcpStream>,
}

fn dial_tcp(addr: &str) -> io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let sa = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("cannot resolve {addr}")))?;
    TcpStream::connect_timeout(&sa, Duration::from_secs(15))
}

/// Connections from a sharing PC that dialed in to this (receiving) PC.
pub struct Inbox {
    rx: Mutex<mpsc::Receiver<TcpStream>>,
}

/// How the receiver reaches the host: by connecting to it, or by waiting
/// for the host to connect in.
#[derive(Clone)]
pub enum Link {
    Dial(String),
    Wait(Arc<Inbox>),
}

impl Link {
    pub fn describe(&self) -> String {
        match self {
            Link::Dial(addr) => addr.clone(),
            Link::Wait(_) => "the sharing PC".into(),
        }
    }
}

/// Accept connections from a sharing PC on `port` (opening it on the router
/// via UPnP) until `stop` is set.
pub fn wait_for_host(port: u16, stop: Arc<AtomicBool>, public: PublicAddr, log: Log) -> io::Result<Link> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    {
        let (stop, public, log) = (stop.clone(), public.clone(), log.clone());
        thread::spawn(move || upnp_keep_open(port, stop, public, log));
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while !stop.load(Relaxed) {
            match listener.accept() {
                Ok((sock, _)) => {
                    if sock.set_nonblocking(false).is_ok() && tx.send(sock).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
                Err(e) => {
                    log(format!("stopped waiting for connections: {e}"));
                    break;
                }
            }
        }
    });
    Ok(Link::Wait(Arc::new(Inbox { rx: Mutex::new(rx) })))
}

/// Log in on a fresh connection to the host.
fn login(s: TcpStream, token: &str, role: Role) -> io::Result<Conn> {
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut c = Conn {
        r: BufReader::with_capacity(1 << 20, s.try_clone()?),
        w: BufWriter::new(s),
    };
    handshake(&mut c.r, &mut c.w, token, role)?;
    c.r.get_ref().set_read_timeout(Some(Duration::from_secs(120)))?;
    Ok(c)
}

fn connect(link: &Link, token: &str) -> io::Result<Conn> {
    match link {
        Link::Dial(addr) => login(dial_tcp(addr)?, token, Role::Dialer),
        Link::Wait(inbox) => loop {
            let s = inbox.rx.lock().unwrap().recv_timeout(Duration::from_secs(20)).map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "the sharing PC has not connected yet")
            })?;
            // Queued connections can go stale (or be from a port scanner); try the next one.
            match login(s, token, Role::Accepter) {
                Err(e) if e.kind() != io::ErrorKind::PermissionDenied => continue,
                r => return r,
            }
        },
    }
}

/// Ask the host what it shares.
pub fn fetch_list(link: &Link, token: &str) -> io::Result<Vec<Entry>> {
    let mut c = connect(link, token)?;
    // Walking a large tree on the host can take a while.
    c.r.get_ref().set_read_timeout(None)?;
    send(&mut c.w, &Req::List)?;
    match recv(&mut c.r, MAX_RESP_FRAME)? {
        Resp::List { files } => Ok(files),
        Resp::Err { msg } => Err(io::Error::other(msg)),
        _ => Err(io::Error::other("protocol error")),
    }
}

fn load_done(path: &Path) -> HashMap<String, u64> {
    let Ok(text) = fs::read_to_string(path) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| {
            let (size, p) = l.split_once('\t')?;
            Some((p.to_string(), size.parse().ok()?))
        })
        .collect()
}

/// Live counters for a running download; read by the UI.
#[derive(Default)]
pub struct Progress {
    pub total_bytes: AtomicU64,
    /// Files skipped because they were already recorded as complete.
    pub already_bytes: AtomicU64,
    pub received: AtomicU64,
    pub hashed: AtomicU64,
    pub reused: AtomicU64,
    pub files_total: AtomicUsize,
    pub files_done: AtomicUsize,
    pub files_failed: AtomicUsize,
    /// What each worker is doing right now (empty = idle).
    pub active: Mutex<Vec<String>>,
}

impl Progress {
    pub fn done_bytes(&self) -> u64 {
        self.already_bytes.load(Relaxed) + self.reused.load(Relaxed) + self.received.load(Relaxed)
    }
}

pub struct SyncJob {
    pub link: Link,
    pub token: String,
    pub dest: PathBuf,
    pub files: Vec<Entry>,
    pub jobs: usize,
    pub recheck: bool,
}

struct Ctx<'a> {
    job: &'a SyncJob,
    queue: Mutex<Vec<Entry>>,
    done_log: Mutex<File>,
    failed: Mutex<Vec<String>>,
    log: Log,
    stop: &'a AtomicBool,
    p: &'a Progress,
}

impl Ctx<'_> {
    fn set_active(&self, slot: usize, what: String) {
        self.p.active.lock().unwrap()[slot] = what;
    }
}

enum Fail {
    /// Connection trouble: reconnect and continue where we left off.
    Net(io::Error),
    /// Give up on this file (host error, local disk error, ...).
    Skip(String),
    Stopped,
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Net(e)
    }
}

/// Download `job.files` into `job.dest`, verifying and reusing whatever is
/// already there. Returns the paths that failed.
pub fn sync(job: &SyncJob, log: Log, stop: &AtomicBool, p: &Progress) -> io::Result<Vec<String>> {
    fs::create_dir_all(&job.dest)?;
    let done_path = job.dest.join(DONE_FILE);
    let done = if job.recheck { HashMap::new() } else { load_done(&done_path) };
    let (complete, mut queue): (Vec<Entry>, Vec<Entry>) = job.files.iter().cloned().partition(|e| {
        done.get(&e.path) == Some(&e.size)
            && safe_join(&job.dest, &e.path).and_then(fs::metadata).map(|m| m.len()).ok() == Some(e.size)
    });
    p.total_bytes.store(job.files.iter().map(|e| e.size).sum(), Relaxed);
    p.already_bytes.store(complete.iter().map(|e| e.size).sum(), Relaxed);
    p.files_total.store(queue.len(), Relaxed);
    log(format!(
        "{} files selected ({}), {} already complete, {} to sync",
        job.files.len(),
        human(p.total_bytes.load(Relaxed)),
        complete.len(),
        queue.len()
    ));
    queue.reverse(); // workers pop from the end; keep alphabetical order

    // A sharing PC that dials in offers only DIAL_CONNS connections.
    let jobs = match job.link {
        Link::Wait(_) => job.jobs.clamp(1, DIAL_CONNS),
        Link::Dial(_) => job.jobs.max(1),
    };
    *p.active.lock().unwrap() = vec![String::new(); jobs];
    let ctx = Ctx {
        job,
        queue: Mutex::new(queue),
        done_log: Mutex::new(OpenOptions::new().create(true).append(true).open(&done_path)?),
        failed: Mutex::new(Vec::new()),
        log,
        stop,
        p,
    };
    thread::scope(|s| {
        for slot in 0..jobs {
            let ctx = &ctx;
            s.spawn(move || worker(ctx, slot));
        }
    });
    Ok(ctx.failed.into_inner().unwrap())
}

/// Connect, retrying until it works. None if stopped or the host refused us.
fn connect_retry(ctx: &Ctx) -> Option<Conn> {
    while !ctx.stop.load(Relaxed) {
        match connect(&ctx.job.link, &ctx.job.token) {
            Ok(c) => return Some(c),
            // When waiting for the host, a bad login may come from a stranger; keep waiting.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && matches!(ctx.job.link, Link::Dial(_)) => {
                (ctx.log)(format!("host refused: {e}"));
                ctx.stop.store(true, Relaxed);
            }
            Err(e) => {
                (ctx.log)(format!("cannot reach {} ({e}), retrying in 5s", ctx.job.link.describe()));
                thread::sleep(Duration::from_secs(5));
            }
        }
    }
    None
}

fn expect_stream(c: &mut Conn, req: &Req) -> Result<u64, Fail> {
    send(&mut c.w, req)?;
    match recv(&mut c.r, MAX_RESP_FRAME)? {
        Resp::Stream { size } => Ok(size),
        Resp::Err { msg } => Err(Fail::Skip(msg)),
        _ => Err(Fail::Net(io::Error::other("protocol error"))),
    }
}

fn worker(ctx: &Ctx, slot: usize) {
    let mut conn: Option<Conn> = None;
    while !ctx.stop.load(Relaxed) {
        let Some(e) = ctx.queue.lock().unwrap().pop() else { break };
        let mut needed = None;
        let mut attempts = 0;
        let result = loop {
            if conn.is_none() {
                match connect_retry(ctx) {
                    Some(c) => conn = Some(c),
                    None => break Err(Fail::Stopped),
                }
            }
            match sync_file(conn.as_mut().unwrap(), ctx, slot, &e, &mut needed) {
                Ok(()) => break Ok(()),
                Err(_) if ctx.stop.load(Relaxed) => break Err(Fail::Stopped),
                Err(Fail::Net(err)) => {
                    conn = None;
                    attempts += 1;
                    if attempts > MAX_RETRIES {
                        break Err(Fail::Skip(format!("network: {err}")));
                    }
                    (ctx.log)(format!("{}: connection problem ({err}), resuming in 5s", e.path));
                    thread::sleep(Duration::from_secs(5));
                }
                Err(other) => {
                    // The stream may be mid-transfer; start fresh for the next file.
                    conn = None;
                    break Err(other);
                }
            }
        };
        match result {
            Ok(()) => {
                ctx.p.files_done.fetch_add(1, Relaxed);
                (ctx.log)(format!("done: {}", e.path));
                let mut log = ctx.done_log.lock().unwrap();
                let _ = writeln!(log, "{}\t{}", e.size, e.path);
            }
            Err(Fail::Skip(msg)) => {
                (ctx.log)(format!("FAILED {}: {msg}", e.path));
                ctx.p.files_failed.fetch_add(1, Relaxed);
                ctx.failed.lock().unwrap().push(e.path.clone());
            }
            Err(_) => break,
        }
    }
    ctx.set_active(slot, String::new());
}

fn append_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Bring one local file in line with the host. `needed` holds the chunk indices
/// still to fetch and survives reconnects, so a retry picks up where it stopped.
fn sync_file(c: &mut Conn, ctx: &Ctx, slot: usize, e: &Entry, needed: &mut Option<Vec<u64>>) -> Result<(), Fail> {
    let dst = safe_join(&ctx.job.dest, &e.path).map_err(|err| Fail::Skip(err.to_string()))?;
    let sk = |err: io::Error| Fail::Skip(format!("{}: {err}", dst.display()));

    if needed.is_none() {
        if !dst.exists() {
            for suf in TORRENT_SUFFIXES {
                let alt = append_suffix(&dst, suf);
                if alt.is_file() {
                    fs::rename(&alt, &dst).map_err(sk)?;
                    (ctx.log)(format!("{}: adopted partial file {}", e.path, alt.display()));
                    break;
                }
            }
        }
        let have = fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        *needed = Some(if have > 0 && e.size > 0 {
            ctx.set_active(slot, format!("checking existing data: {}", e.path));
            let bad = compare(c, ctx, e, &dst)?;
            let bad_bytes: u64 = bad.iter().map(|&i| chunk_len(e.size, i) as u64).sum();
            ctx.p.reused.fetch_add(e.size - bad_bytes, Relaxed);
            (ctx.log)(format!(
                "{}: {} of {} chunks need download ({})",
                e.path,
                bad.len(),
                n_chunks(e.size),
                human(bad_bytes)
            ));
            bad
        } else {
            (0..n_chunks(e.size)).collect()
        });
    }
    let todo = needed.as_mut().unwrap();

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(sk)?;
    }
    let mut f = OpenOptions::new().write(true).create(true).truncate(false).open(&dst).map_err(sk)?;
    if f.metadata().map_err(sk)?.len() != e.size {
        f.set_len(e.size).map_err(sk)?;
    }

    if !todo.is_empty() {
        ctx.set_active(slot, format!("downloading: {}", e.path));
        let size = expect_stream(c, &Req::Get { path: e.path.clone(), chunks: todo.clone() })?;
        if size != e.size {
            return Err(Fail::Skip("file changed on host since listing; run again".into()));
        }
        let mut buf = vec![0u8; CHUNK as usize];
        let mut hash = [0u8; 32];
        let mut done = 0;
        let res = (|| -> Result<(), Fail> {
            for &i in todo.iter() {
                if ctx.stop.load(Relaxed) {
                    return Err(Fail::Stopped);
                }
                let len = chunk_len(e.size, i);
                c.r.read_exact(&mut hash)?;
                c.r.read_exact(&mut buf[..len])?;
                if blake3::hash(&buf[..len]).as_bytes() != &hash {
                    return Err(Fail::Net(io::Error::other("chunk checksum mismatch")));
                }
                f.seek(SeekFrom::Start(i * CHUNK)).and_then(|_| f.write_all(&buf[..len])).map_err(sk)?;
                ctx.p.received.fetch_add(len as u64, Relaxed);
                done += 1;
            }
            Ok(())
        })();
        todo.drain(..done);
        res?;
    }
    f.sync_all().map_err(sk)?;
    Ok(())
}

/// Hash the local copy and ask the host for its hashes at the same time;
/// return the indices of chunks that are missing or differ.
fn compare(c: &mut Conn, ctx: &Ctx, e: &Entry, dst: &Path) -> Result<Vec<u64>, Fail> {
    (ctx.log)(format!("{}: checking existing local data...", e.path));
    thread::scope(|s| {
        let local = s.spawn(|| -> io::Result<Vec<Option<[u8; 32]>>> {
            let mut f = File::open(dst)?;
            let mut v = Vec::new();
            hash_chunks(&mut f, e.size, |h, len| {
                if ctx.stop.load(Relaxed) {
                    return Err(io::Error::other("stopped"));
                }
                if h.is_some() {
                    ctx.p.hashed.fetch_add(len as u64, Relaxed);
                }
                v.push(h);
                Ok(())
            })?;
            Ok(v)
        });
        let remote = (|| -> Result<Vec<[u8; 32]>, Fail> {
            let size = expect_stream(c, &Req::Hashes { path: e.path.clone() })?;
            if size != e.size {
                return Err(Fail::Skip("file changed on host since listing; run again".into()));
            }
            let mut v = vec![[0u8; 32]; n_chunks(size) as usize];
            for h in v.iter_mut() {
                if ctx.stop.load(Relaxed) {
                    return Err(Fail::Stopped);
                }
                c.r.read_exact(h)?;
            }
            Ok(v)
        })();
        let local = local
            .join()
            .unwrap()
            .map_err(|err| Fail::Skip(format!("reading {}: {err}", dst.display())))?;
        let remote = remote?;
        let mut bad = Vec::new();
        for (i, (r, l)) in remote.iter().zip(&local).enumerate() {
            if l.as_ref() != Some(r) {
                bad.push(i as u64);
            }
        }
        Ok(bad)
    })
}
