use crate::crypto::{ct_eq, hex, hmac, random32, sha256, unhex32};
use crate::{
    Result,
    config::{Config, Mode},
    store,
};
use std::collections::VecDeque;
use std::env;
use std::fs;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};
const MAX_FRAME: usize = 512;
const MAX_CLIENTS: usize = 4;
const TURN_TIME: Duration = Duration::from_secs(10);
const AUTH_TIME: Duration = Duration::from_secs(3);
const SESSION_TIME: Duration = Duration::from_secs(300);

fn mac(key: &[u8; 32], fields: &[&str]) -> [u8; 32] {
    let mut bytes = Vec::new();
    for field in fields {
        bytes.extend_from_slice(field.as_bytes());
        bytes.push(0);
    }
    hmac(key, &bytes)
}

fn epoch() -> Result<String> {
    #[cfg(target_os = "linux")]
    let id = {
        let mut s = String::new();
        File::open("/proc/sys/kernel/random/boot_id")
            .map_err(|_| "boot_id")?
            .take(128)
            .read_to_string(&mut s)
            .map_err(|_| "boot_id")?;
        s
    };
    #[cfg(target_os = "macos")]
    let id = {
        let out = Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .map_err(|_| "boot_id")?;
        if !out.status.success() {
            return Err("boot_id");
        }
        String::from_utf8(out.stdout).map_err(|_| "boot_id")?
    };
    let id = id.trim();
    if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err("boot_id");
    }
    Ok(hex(&sha256(id.as_bytes())))
}

fn starttime() -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        let s = fs::read_to_string("/proc/self/stat").map_err(|_| "proc_identity")?;
        s.rsplit_once(')')
            .and_then(|(_, s)| s.split_whitespace().nth(19))
            .map(str::to_owned)
            .ok_or("proc_identity")
    }
    #[cfg(not(target_os = "linux"))]
    Ok("native-test".to_owned())
}

fn face_name(s: &str) -> Result<&'static str> {
    match s {
        "Happy" | "KaiXin" => Ok("Happy"),
        "GetAngry" | "ShengQi" => Ok("GetAngry"),
        "Worry" | "DanYou" => Ok("Worry"),
        "Default" => Ok("Default"),
        "Listen" => Ok("Listen"),
        "ThinkDeep" => Ok("ThinkDeep"),
        "Sad" => Ok("Sad"),
        "Like" => Ok("Like"),
        _ => Err("unsupported_face"),
    }
}
fn face(s: &str, config: &Config) -> Result<String> {
    let local = config.face.as_ref().ok_or("face_disabled")?;
    let name = face_name(s)?;
    let mut child = Command::new(&local.executable)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args([
            &format!("--bus={}", local.address),
            "--type=signal",
            "/xapp/xuanyuan/interface",
            "com.xuanyuan.interface.EmojiPlaySignal",
            &format!("string:{name}"),
            "int32:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "dbus_spawn")?;
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                return if s.success() {
                    Ok(format!("sent:{name}"))
                } else {
                    Err("dbus_exit")
                };
            }
            Ok(None) if Instant::now() < end => thread::sleep(Duration::from_millis(10)),
            _ => break,
        }
    }
    let _ = child.kill();
    let cleanup = Instant::now() + Duration::from_millis(300);
    while Instant::now() < cleanup {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err("dbus_timeout");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err("child_cleanup_failed")
}

struct Auth {
    id: String,
    key: [u8; 32],
    client: String,
    turn: String,
}
struct Conn {
    socket: TcpStream,
    server: String,
    auth: Option<Auth>,
    input: Vec<u8>,
    output: Vec<u8>,
    offset: usize,
    write_end: Instant,
    deadline: Instant,
    end: Instant,
}
impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
impl Conn {
    fn new(socket: TcpStream, epoch: &str, instance: &str) -> Result<Self> {
        socket.set_nonblocking(true).map_err(|_| "socket")?;
        let nonce = hex(&random32()?);
        let now = Instant::now();
        Ok(Self {
            socket,
            server: nonce.clone(),
            auth: None,
            input: Vec::new(),
            output: format!("N3B1 {epoch} {instance} {nonce}\n").into_bytes(),
            offset: 0,
            write_end: now + AUTH_TIME,
            deadline: now + AUTH_TIME,
            end: now + SESSION_TIME,
        })
    }
    fn queue(&mut self, s: String) {
        self.output = s.into_bytes();
        self.offset = 0;
        self.write_end = Instant::now() + Duration::from_secs(2);
    }
}

fn tokens(line: &[u8]) -> Result<Vec<&str>> {
    let s = std::str::from_utf8(line).map_err(|_| "frame")?;
    if !s.bytes().all(|b| (0x21..=0x7e).contains(&b) || b == b' ') {
        return Err("frame");
    }
    let v: Vec<_> = s.split(' ').collect();
    if v.iter().any(|s| s.is_empty()) {
        return Err("frame");
    }
    Ok(v)
}

// All persistent writes and ability calls run in this single event loop.
// Store failure aborts the daemon: never continue with uncertain disk state.
fn command(
    c: &mut Conn,
    line: &[u8],
    keys: &mut store::Store,
    ep: &str,
    instance: &str,
    config: &Config,
) -> Result<bool> {
    let t = match tokens(line) {
        Ok(t) => t,
        Err(_) => return Ok(false),
    };
    if c.auth.is_none() {
        if t.len() != 4 || t[0] != "AUTH" || !store::host_id(t[1]) {
            return Ok(false);
        }
        if unhex32(t[2]).is_err() {
            return Ok(false);
        }
        let proof = match unhex32(t[3]) {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        let h = match keys.find(t[1]) {
            Some(h) if !h.revoked => h,
            _ => return Ok(false),
        };
        let expected = mac(
            &h.key,
            &["node3b/auth/v1", ep, instance, &c.server, &h.id, t[2]],
        );
        if !ct_eq(&expected, &proof) {
            return Ok(false);
        }
        let last = h.last_seq.to_string();
        let turn = hex(&random32()?);
        let ack = hex(&mac(
            &h.key,
            &[
                "node3b/ready/v1",
                ep,
                instance,
                &c.server,
                &h.id,
                t[2],
                &last,
                &turn,
            ],
        ));
        let a = Auth {
            id: h.id.clone(),
            key: h.key,
            client: t[2].to_owned(),
            turn: turn.clone(),
        };
        c.queue(format!("READY {last} {turn} {ack}\n"));
        c.auth = Some(a);
        c.deadline = Instant::now() + TURN_TIME;
        return Ok(true);
    }
    let a = c.auth.as_ref().ok_or("internal")?;
    if t.len() != 5 || t[0] != "CMD" {
        return Ok(false);
    }
    let seq = match store::canonical_seq(t[1], false) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    let proof = match unhex32(t[4]) {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };
    let h = match keys.find(&a.id) {
        Some(h) if !h.revoked && ct_eq(&h.key, &a.key) => h,
        _ => return Ok(false),
    };
    let expected = mac(
        &a.key,
        &[
            "node3b/cmd/v1",
            ep,
            instance,
            &c.server,
            &a.id,
            &a.client,
            &a.turn,
            t[1],
            t[2],
            t[3],
        ],
    );
    if !ct_eq(&expected, &proof) || seq <= h.last_seq {
        return Ok(false);
    }
    // Each turn expires locally. Reserve seq before any ability side effect.
    if Instant::now() >= c.deadline {
        return Ok(false);
    }
    let next_turn = hex(&random32()?);
    let id = a.id.clone();
    let key = a.key;
    let client = a.client.clone();
    let old_turn = a.turn.clone();
    if config.cancelled() {
        return Ok(false);
    }
    keys.reserve(&id, seq)?;
    if config.cancelled() {
        return Ok(false);
    }
    let outcome = match (t[2], t[3]) {
        ("info", "-") => Ok(format!(
            "version={},arch={},os={}",
            env!("CARGO_PKG_VERSION"),
            env::consts::ARCH,
            env::consts::OS
        )),
        ("capabilities", "-") => Ok(if config.face.is_some() {
            "face,info,capabilities"
        } else {
            "info,capabilities"
        }
        .to_owned()),
        ("face", name) => face(name, config),
        _ => Err("unsupported_command"),
    };
    if matches!(outcome, Err("child_cleanup_failed")) {
        return Err("child_cleanup_failed");
    }
    let (status, payload) = match outcome {
        Ok(p) => ("OK", p),
        Err(e) => ("ERR", e.to_owned()),
    };
    let proof = hex(&mac(
        &key,
        &[
            "node3b/result/v1",
            ep,
            instance,
            &c.server,
            &id,
            &client,
            &old_turn,
            t[1],
            status,
            &payload,
            &next_turn,
        ],
    ));
    c.auth.as_mut().ok_or("internal")?.turn = next_turn.clone();
    c.queue(format!(
        "RESULT {} {status} {payload} {next_turn} {proof}\n",
        t[1]
    ));
    c.deadline = Instant::now() + TURN_TIME;
    Ok(true)
}

fn poll(
    c: &mut Conn,
    keys: &mut store::Store,
    ep: &str,
    instance: &str,
    config: &Config,
) -> Result<bool> {
    let now = Instant::now();
    if now >= c.deadline || now >= c.end {
        return Ok(false);
    }
    if let Some(a) = &c.auth
        && !matches!(keys.find(&a.id),Some(h) if !h.revoked && ct_eq(&h.key,&a.key))
    {
        return Ok(false);
    }
    if c.offset < c.output.len() {
        if now >= c.write_end {
            return Ok(false);
        }
        match c.socket.write(&c.output[c.offset..]) {
            Ok(0) => return Ok(false),
            Ok(n) => c.offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(_) => return Ok(false),
        }
        if c.offset < c.output.len() {
            return Ok(true);
        }
        c.output.clear();
        c.offset = 0;
    }
    let mut buf = [0u8; MAX_FRAME + 1];
    let room = MAX_FRAME + 1 - c.input.len();
    match c.socket.read(&mut buf[..room]) {
        Ok(0) => return Ok(false),
        Ok(n) => c.input.extend_from_slice(&buf[..n]),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
        Err(_) => return Ok(false),
    }
    if c.input.len() > MAX_FRAME {
        return Ok(false);
    }
    if let Some(n) = c.input.iter().position(|&b| b == b'\n') {
        if n + 1 != c.input.len() {
            return Ok(false);
        } // no pipelining or command queue
        let line = c.input[..n].to_vec();
        c.input.clear();
        return command(c, &line, keys, ep, instance, config);
    }
    Ok(true)
}

fn unix_line(stream: &mut UnixStream, limit: usize, end: Instant) -> Result<String> {
    stream.set_nonblocking(true).map_err(|_| "admin_socket")?;
    let mut line = Vec::new();
    while line.len() < limit && Instant::now() < end {
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => return Err("admin_eof"),
            Ok(_) if byte[0] == b'\n' => return String::from_utf8(line).map_err(|_| "admin_frame"),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(_) => return Err("admin_read"),
        }
    }
    Err("admin_deadline_or_size")
}
fn unix_write(stream: &mut UnixStream, data: &[u8]) -> Result<()> {
    stream.set_nonblocking(true).map_err(|_| "admin_socket")?;
    let end = Instant::now() + Duration::from_secs(1);
    let mut offset = 0;
    while offset < data.len() && Instant::now() < end {
        match stream.write(&data[offset..]) {
            Ok(0) => return Err("admin_write"),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(_) => return Err("admin_write"),
        }
    }
    if offset == data.len() {
        Ok(())
    } else {
        Err("admin_write_deadline")
    }
}

fn admins(
    listener: &UnixListener,
    conns: &mut VecDeque<Conn>,
    keys: &mut store::Store,
    ep: &str,
    instance: &str,
) -> Result<bool> {
    for _ in 0..4 {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return Err("admin_accept"),
        };
        let line = match unix_line(&mut stream, 192, Instant::now() + Duration::from_secs(1)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let t: Vec<_> = line.split(' ').collect();
        // This private local endpoint binds shutdown to this process instance;
        // it never resolves a numeric PID for signaling another process.
        if t == ["STOP", ep, instance, &std::process::id().to_string()] {
            let _ = unix_write(&mut stream, b"STOPPING\n");
            return Ok(true);
        }
        let reply = match t.as_slice() {
            ["PING"] => format!("PONG {ep} {instance} {}\n", std::process::id()),
            ["PAIR", id] if store::host_id(id) => {
                if keys.find(id).is_none() && keys.hosts.len() >= store::MAX_HOSTS {
                    "ERR host_limit\n".to_owned()
                } else {
                    let result = keys.pair(id)?;
                    conns.retain(|c| !matches!(&c.auth,Some(a) if a.id==*id));
                    result
                }
            }
            ["REVOKE", id] if store::host_id(id) => {
                if keys.find(id).is_none() {
                    "ERR unknown_host\n".to_owned()
                } else {
                    keys.revoke(id)?;
                    conns.retain(|c| !matches!(&c.auth,Some(a) if a.id==*id));
                    "REVOKED\n".to_owned()
                }
            }
            _ => "ERR admin_command\n".to_owned(),
        };
        // Revocation/re-pair commits + closes all old authenticated connections
        // BEFORE this acknowledgement; one in-progress command may finish first.
        let _ = unix_write(&mut stream, reply.as_bytes());
    }
    Ok(false)
}

// A peer abort is local to that connection. Every attempt, including errors,
// consumes the fixed iteration budget. Unclassified errors remain fatal.
fn accept_batch<T>(
    mut accept: impl FnMut() -> std::io::Result<T>,
    mut consume: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    for _ in 0..8 {
        match accept() {
            Ok(item) => consume(item)?,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                continue;
            }
            Err(_) => return Err("accept_failed"),
        }
    }
    Ok(())
}

struct Runtime {
    socket: PathBuf,
    receipt: PathBuf,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.receipt);
    }
}
fn serve(config: &Config) -> Result<()> {
    let dir = config.data_dir.as_deref().ok_or("data_dir_required")?;
    let listen = config.listen.ok_or("listen_required")?;
    if config.cancelled() {
        return Err("owner_cancelled");
    }
    let _lock = store::lock(dir)?.ok_or("already_running")?;
    let mut keys = store::Store::load(dir, false)?;
    let ep = epoch()?;
    let instance = hex(&random32()?);
    let tcp = TcpListener::bind(listen).map_err(|_| "bind_failed")?;
    let listen = tcp.local_addr().map_err(|_| "socket")?;
    tcp.set_nonblocking(true).map_err(|_| "socket")?;
    let sock = dir.join("admin.sock");
    if let Ok(m) = fs::symlink_metadata(&sock) {
        if !m.file_type().is_socket() {
            return Err("admin_path");
        }
        fs::remove_file(&sock).map_err(|_| "admin_unlink")?;
    }
    let admin = UnixListener::bind(&sock).map_err(|_| "admin_bind")?;
    let _runtime = Runtime {
        socket: sock.clone(),
        receipt: dir.join("runtime.json"),
    };
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))
        .map_err(|_| "admin_permissions")?;
    admin.set_nonblocking(true).map_err(|_| "admin_socket")?;
    store::atomic_write(&dir.join("runtime.json"),format!("{{\"pid\":{},\"epoch\":\"{ep}\",\"instance\":\"{instance}\",\"listen\":\"{listen}\",\"starttime\":\"{}\"}}\n",std::process::id(),starttime()?).as_bytes())?;
    let _ = writeln!(std::io::stdout(), "READY yonbo-node {listen}");
    let mut conns = VecDeque::new();
    loop {
        if config.cancelled() {
            return Ok(());
        }
        if admins(&admin, &mut conns, &mut keys, &ep, &instance)? {
            return Ok(());
        }
        accept_batch(
            || tcp.accept(),
            |(socket, _)| {
                if conns.len() < MAX_CLIENTS {
                    conns.push_back(Conn::new(socket, &ep, &instance)?);
                } else {
                    let _ = socket.shutdown(Shutdown::Both);
                }
                Ok(())
            },
        )?;
        let count = conns.len();
        for _ in 0..count {
            if admins(&admin, &mut conns, &mut keys, &ep, &instance)? {
                return Ok(());
            }
            if let Some(mut c) = conns.pop_front()
                && poll(&mut c, &mut keys, &ep, &instance, config)?
            {
                conns.push_back(c);
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn management(dir: &Path, verb: &str, id: &str) -> Result<()> {
    if !store::host_id(id) {
        return Err("host_id");
    }
    let output = if let Some(_lock) = store::lock(dir)? {
        let mut keys = store::Store::load(dir, verb == "PAIR")?;
        if verb == "PAIR" {
            keys.pair(id)?
        } else {
            keys.revoke(id)?;
            "REVOKED\n".to_owned()
        }
    } else {
        let mut s = UnixStream::connect(dir.join("admin.sock")).map_err(|_| "admin_unavailable")?;
        unix_write(&mut s, format!("{verb} {id}\n").as_bytes())?;
        let result = unix_line(&mut s, 512, Instant::now() + Duration::from_secs(5))?;
        format!("{result}\n")
    };
    if output.starts_with("ERR ") {
        return Err("admin_rejected");
    }
    // Pair JSON is secret: only the SSH pairing helper captures it directly to
    // a 0600 host file. Do not invoke --pair through a logging shell/terminal.
    std::io::stdout()
        .write_all(output.as_bytes())
        .map_err(|_| "output")
}

fn run() -> Result<()> {
    let raw: Vec<_> = env::args_os().skip(1).collect();
    let args: Option<Vec<_>> = raw.iter().map(|a| a.to_str()).collect();
    let args = args.ok_or("arguments")?;
    let config = Config::parse(&args)?;
    match &config.mode {
        Mode::Serve => serve(&config),
        Mode::Pair(id) => management(
            config.data_dir.as_deref().ok_or("data_dir_required")?,
            "PAIR",
            id,
        ),
        Mode::Revoke(id) => management(
            config.data_dir.as_deref().ok_or("data_dir_required")?,
            "REVOKE",
            id,
        ),
        Mode::Info => writeln!(
            std::io::stdout(),
            "yonbo-node={} arch={} os={} epoch={}",
            env!("CARGO_PKG_VERSION"),
            env::consts::ARCH,
            env::consts::OS,
            epoch()?
        )
        .map_err(|_| "output"),
    }
}
pub fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|_| {}));
    match std::panic::catch_unwind(run) {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(e)) => {
            let _ = writeln!(std::io::stderr(), "ERR {e}");
            ExitCode::from(1)
        }
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "ERR internal_failure");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    #[test]
    fn aborted_accept_is_recoverable_and_bounded() {
        use std::io::{Error, ErrorKind};
        let mut calls = 0;
        let mut received = Vec::new();
        accept_batch(
            || {
                calls += 1;
                match calls {
                    1 => Err(Error::from(ErrorKind::ConnectionAborted)),
                    2 => Ok(42),
                    _ => Err(Error::from(ErrorKind::WouldBlock)),
                }
            },
            |v| {
                received.push(v);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(received, vec![42]);
        assert_eq!(calls, 3);
        calls = 0;
        accept_batch::<()>(
            || {
                calls += 1;
                Err(Error::from(ErrorKind::ConnectionAborted))
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(calls, 8);
        assert_eq!(
            accept_batch::<()>(|| Err(Error::from(ErrorKind::PermissionDenied)), |_| Ok(())),
            Err("accept_failed")
        );
    }
    #[test]
    fn admin_reply_survives_peer_close() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server.write_all(b"REVOKED\n").unwrap();
        drop(server);
        assert_eq!(
            unix_line(&mut client, 128, Instant::now() + Duration::from_secs(1)).unwrap(),
            "REVOKED"
        );
    }
}
