use crate::Result;
use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug)]
pub enum Mode {
    Serve,
    Pair(String),
    Revoke(String),
    Info,
}
#[derive(Debug)]
pub struct Face {
    pub executable: PathBuf,
    pub address: String,
}
#[derive(Debug)]
pub struct Owner {
    pub enabled: PathBuf,
    pub cancel: PathBuf,
}
#[derive(Debug)]
pub struct Config {
    pub mode: Mode,
    pub data_dir: Option<PathBuf>,
    pub listen: Option<SocketAddr>,
    pub face: Option<Face>,
    pub owner: Option<Owner>,
}

fn absolute(s: &str) -> Result<PathBuf> {
    let p = PathBuf::from(s);
    if !p.is_absolute() || s.contains('\0') {
        return Err("absolute_path_required");
    }
    Ok(p)
}
impl Config {
    pub fn parse(args: &[&str]) -> Result<Self> {
        let (mut data_dir, mut listen, mut executable, mut address) = (None, None, None, None);
        let (mut enabled, mut cancel) = (None, None);
        let mut seen = HashSet::new();
        let mut i = 0;
        while i < args.len() && !matches!(args[i], "--serve" | "--pair" | "--revoke" | "--info") {
            let flag = args[i];
            if !seen.insert(flag) {
                return Err("duplicate_option");
            }
            let value = *args.get(i + 1).ok_or("option_value_required")?;
            match flag {
                "--data-dir" => data_dir = Some(absolute(value)?),
                "--listen" => listen = Some(value.parse().map_err(|_| "listen_address")?),
                "--dbus-send" => executable = Some(absolute(value)?),
                "--dbus-address" => {
                    // Local Unix-path transport only; no remote bus/network fallback.
                    let path = value.strip_prefix("unix:path=").ok_or("dbus_address")?;
                    absolute(path)?;
                    if value.len() > 256
                        || value
                            .bytes()
                            .any(|b| b.is_ascii_control() || matches!(b, b',' | b';'))
                    {
                        return Err("dbus_address");
                    }
                    address = Some(value.to_owned());
                }
                "--owner-enabled" => enabled = Some(absolute(value)?),
                "--owner-cancel" => cancel = Some(absolute(value)?),
                _ => return Err("unknown_option"),
            }
            i += 2;
        }
        let mode = match &args[i..] {
            ["--serve"] => Mode::Serve,
            ["--info"] => Mode::Info,
            ["--pair", id] if crate::store::host_id(id) => Mode::Pair((*id).to_owned()),
            ["--revoke", id] if crate::store::host_id(id) => Mode::Revoke((*id).to_owned()),
            _ => return Err("usage"),
        };
        if !matches!(mode, Mode::Info) && data_dir.is_none() {
            return Err("data_dir_required");
        }
        if matches!(mode, Mode::Info) && data_dir.is_some() {
            return Err("unused_option");
        }
        if matches!(mode, Mode::Serve) {
            if listen.is_none() {
                return Err("listen_required");
            }
        } else if listen.is_some()
            || executable.is_some()
            || address.is_some()
            || enabled.is_some()
            || cancel.is_some()
        {
            return Err("unused_option");
        }
        let face = match (executable, address) {
            (None, None) => None,
            (Some(executable), Some(address)) => Some(Face {
                executable,
                address,
            }),
            _ => return Err("face_config_incomplete"),
        };
        let owner = match (enabled, cancel) {
            (None, None) => None,
            (Some(enabled), Some(cancel)) => Some(Owner { enabled, cancel }),
            _ => return Err("owner_config_incomplete"),
        };
        Ok(Self {
            mode,
            data_dir,
            listen,
            face,
            owner,
        })
    }

    pub fn cancelled(&self) -> bool {
        let Some(owner) = &self.owner else {
            return false;
        };
        if !fs::symlink_metadata(&owner.enabled).is_ok_and(|m| m.is_file()) {
            return true;
        }
        // An unreadable marker is not evidence of permission to continue.
        !matches!(fs::symlink_metadata(&owner.cancel), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_ambiguous_configuration_is_closed() {
        for args in [
            vec![],
            vec!["--serve"],
            vec!["--pair", "operator"],
            vec!["--data-dir", "relative", "--serve"],
            vec!["--data-dir", "/tmp/node", "--serve"],
            vec!["--listen", "127.0.0.1:0", "--info"],
            vec![
                "--data-dir",
                "/tmp/node",
                "--data-dir",
                "/tmp/other",
                "--pair",
                "operator",
            ],
            vec![
                "--data-dir",
                "/tmp/node",
                "--listen",
                "127.0.0.1:0",
                "--owner-enabled",
                "/tmp/enabled",
                "--serve",
            ],
            vec![
                "--data-dir",
                "/tmp/node",
                "--listen",
                "127.0.0.1:0",
                "--dbus-send",
                "/usr/bin/dbus-send",
                "--serve",
            ],
        ] {
            assert!(Config::parse(&args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn explicit_listener_does_not_grant_display_or_lifecycle() {
        let config = Config::parse(&[
            "--data-dir",
            "/tmp/node",
            "--listen",
            "127.0.0.1:0",
            "--serve",
        ])
        .unwrap();
        assert!(config.face.is_none());
        assert!(config.owner.is_none());
        assert!(!config.cancelled());
    }
}
