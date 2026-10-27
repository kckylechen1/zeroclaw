use crate::Result;
use crate::crypto::{hex, random32, unhex32};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

unsafe extern "C" {
    fn geteuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
}

pub const MAX_HOSTS: usize = 32;
const MAX_STORE: usize = 16384;
pub struct Lock {
    _file: File,
}

pub fn secure_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|_| "directory_create")?;
    }
    let m = fs::symlink_metadata(dir).map_err(|_| "directory_read")?;
    // Only the current OS account may control the directory; root on device.
    if !m.is_dir() || m.uid() != unsafe { geteuid() } || m.mode() & 0o777 != 0o700 {
        return Err("directory_permissions");
    }
    Ok(())
}

pub fn checked_file(path: &Path) -> Result<File> {
    let m = fs::symlink_metadata(path).map_err(|_| "file_missing")?;
    if !m.is_file() || m.uid() != unsafe { geteuid() } || m.mode() & 0o777 != 0o600 {
        return Err("file_permissions");
    }
    let f = File::open(path).map_err(|_| "file_open")?;
    let opened = f.metadata().map_err(|_| "file_metadata")?;
    if m.ino() != opened.ino() || m.dev() != opened.dev() {
        return Err("file_changed");
    }
    Ok(f)
}

pub fn lock(dir: &Path) -> Result<Option<Lock>> {
    secure_dir(dir)?;
    let path = dir.join(".node.lock");
    if !path.exists() {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err("lock_create"),
        }
    }
    let file = checked_file(&path)?;
    // flock is held across the entire daemon lifetime/offline transaction.
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } == 0 {
        Ok(Some(Lock { _file: file }))
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err("lock_failed")
    }
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("bad_path")?;
    let tmp = parent.join(format!(
        ".write-{}-{}",
        std::process::id(),
        hex(&random32()?[..8])
    ));
    let operation = || -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|_| "store_create")?;
        f.write_all(contents).map_err(|_| "store_write")?;
        f.sync_all().map_err(|_| "store_sync")?;
        fs::rename(&tmp, path).map_err(|_| "store_rename")?;
        File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|_| "directory_sync")?;
        Ok(())
    };
    let result = operation();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn host_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[derive(Clone)]
pub struct Host {
    pub id: String,
    pub key: [u8; 32],
    pub revoked: bool,
    pub last_seq: u64,
}
pub struct Store {
    path: PathBuf,
    pub hosts: Vec<Host>,
}

impl Store {
    pub fn load(dir: &Path, allow_new: bool) -> Result<Self> {
        let path = dir.join("keys.json");
        let hosts = if !path.exists() && allow_new {
            Vec::new()
        } else {
            let mut bytes = Vec::new();
            checked_file(&path)?
                .take((MAX_STORE + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| "store_read")?;
            if bytes.len() > MAX_STORE {
                return Err("store_size");
            }
            parse_store(&bytes)?
        };
        Ok(Self { path, hosts })
    }
    pub fn find(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }
    fn commit(&mut self, next: Vec<Host>) -> Result<()> {
        let mut s = String::from("{\"version\":1,\"hosts\":[");
        for (i, h) in next.iter().enumerate() {
            if i != 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"id\":\"{}\",\"key\":\"{}\",\"revoked\":{},\"last_seq\":{}}}",
                h.id,
                hex(&h.key),
                h.revoked,
                h.last_seq
            ));
        }
        s.push_str("]}\n");
        atomic_write(&self.path, s.as_bytes())?;
        self.hosts = next;
        Ok(())
    }
    pub fn pair(&mut self, id: &str) -> Result<String> {
        if !host_id(id) {
            return Err("host_id");
        }
        let host = Host {
            id: id.to_owned(),
            key: random32()?,
            revoked: false,
            last_seq: 0,
        };
        let export = format!(
            "{{\"version\":1,\"host\":\"{}\",\"key\":\"{}\",\"last_seq\":0}}\n",
            id,
            hex(&host.key)
        );
        let mut next = self.hosts.clone();
        if let Some(old) = next.iter_mut().find(|h| h.id == id) {
            *old = host;
        } else {
            if next.len() == MAX_HOSTS {
                return Err("host_limit");
            }
            next.push(host);
        }
        self.commit(next)?;
        Ok(export)
    }
    pub fn revoke(&mut self, id: &str) -> Result<()> {
        if !host_id(id) {
            return Err("host_id");
        }
        let mut next = self.hosts.clone();
        let h = next.iter_mut().find(|h| h.id == id).ok_or("unknown_host")?;
        h.revoked = true;
        self.commit(next)
    }
    pub fn reserve(&mut self, id: &str, seq: u64) -> Result<()> {
        let mut next = self.hosts.clone();
        let h = next.iter_mut().find(|h| h.id == id).ok_or("auth")?;
        if h.revoked || seq <= h.last_seq {
            return Err("seq");
        }
        h.last_seq = seq;
        self.commit(next)
    }
}

// A deliberately small JSON grammar for our own versioned store, not a general
// parser: canonical fields/order, no escaped strings, bounded hosts/lengths,
// duplicate/extra fields and trailing bytes rejected. Invalid data fails closed.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}
impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }
    fn token(&mut self, t: &[u8]) -> Result<()> {
        self.ws();
        if self.b.get(self.i..self.i + t.len()) != Some(t) {
            return Err("store_format");
        }
        self.i += t.len();
        Ok(())
    }
    fn string(&mut self) -> Result<String> {
        self.token(b"\"")?;
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i] != b'"' {
            let c = self.b[self.i];
            if !(0x20..=0x7e).contains(&c) || c == b'\\' || self.i - start >= 64 {
                return Err("store_string");
            }
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| "store_string")?
            .to_owned();
        self.token(b"\"")?;
        Ok(s)
    }
    fn number(&mut self) -> Result<u64> {
        self.ws();
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "store_number")?;
        canonical_seq(s, true)
    }
}
pub fn canonical_seq(s: &str, zero: bool) -> Result<u64> {
    if s.is_empty()
        || s.len() > 20
        || !s.bytes().all(|b| b.is_ascii_digit())
        || (s.len() > 1 && s.starts_with('0'))
    {
        return Err("seq");
    }
    let n = s.parse::<u64>().map_err(|_| "seq")?;
    if !zero && n == 0 {
        return Err("seq");
    }
    Ok(n)
}
fn parse_store(bytes: &[u8]) -> Result<Vec<Host>> {
    let mut p = Parser { b: bytes, i: 0 };
    let mut hosts: Vec<Host> = Vec::new();
    p.token(b"{")?;
    p.token(b"\"version\"")?;
    p.token(b":")?;
    p.token(b"1")?;
    p.token(b",")?;
    p.token(b"\"hosts\"")?;
    p.token(b":")?;
    p.token(b"[")?;
    p.ws();
    while p.b.get(p.i) != Some(&b']') {
        if !hosts.is_empty() {
            p.token(b",")?;
        }
        if hosts.len() >= MAX_HOSTS {
            return Err("host_limit");
        }
        p.token(b"{")?;
        p.token(b"\"id\"")?;
        p.token(b":")?;
        let id = p.string()?;
        if !host_id(&id) || hosts.iter().any(|h| h.id == id) {
            return Err("store_host");
        }
        p.token(b",")?;
        p.token(b"\"key\"")?;
        p.token(b":")?;
        let key = unhex32(&p.string()?)?;
        p.token(b",")?;
        p.token(b"\"revoked\"")?;
        p.token(b":")?;
        p.ws();
        let revoked = p.b.get(p.i) == Some(&b't');
        p.token(if revoked { b"true" } else { b"false" })?;
        p.token(b",")?;
        p.token(b"\"last_seq\"")?;
        p.token(b":")?;
        let last_seq = p.number()?;
        p.token(b"}")?;
        hosts.push(Host {
            id,
            key,
            revoked,
            last_seq,
        });
        p.ws();
    }
    p.token(b"]")?;
    p.token(b"}")?;
    p.ws();
    if p.i != p.b.len() {
        return Err("store_trailing");
    }
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_ambiguous_store() {
        let good = format!(
            "{{\"version\":1,\"hosts\":[{{\"id\":\"test\",\"key\":\"{}\",\"revoked\":false,\"last_seq\":0}}]}}",
            "00".repeat(32)
        );
        assert_eq!(parse_store(good.as_bytes()).unwrap().len(), 1);
        for bad in [
            good.replace("\"version\":1", "\"version\":11"),
            good.replace("\"last_seq\":0", "\"last_seq\":00"),
            format!("{good}x"),
            good.replace("false", "falseX"),
            good.replace("\"test\"", "\"../test\""),
        ] {
            assert!(parse_store(bad.as_bytes()).is_err());
        }
    }
}
