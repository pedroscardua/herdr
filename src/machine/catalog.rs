use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_CATALOG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Machine {
    pub id: String,
    pub label: String,
    pub target: String,
    pub session: String,
    pub enabled: bool,
    #[serde(default = "current_protocol")]
    pub protocol: u32,
}

fn current_protocol() -> u32 {
    crate::protocol::PROTOCOL_VERSION
}

impl Machine {
    pub fn new(label: String, target: String, session: String) -> io::Result<Self> {
        let machine = Self {
            id: format!("{:032x}", rand::random::<u128>()),
            label,
            target,
            session,
            enabled: true,
            protocol: current_protocol(),
        };
        machine.validate()?;
        Ok(machine)
    }

    pub fn validate(&self) -> io::Result<()> {
        let invalid = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
        if !matches!(self.protocol, 12 | 13) {
            return Err(invalid("unsupported motionharvest wire protocol"));
        }
        if self.id.len() != 32 || !self.id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid("invalid machine ID"));
        }
        if self.label.trim().is_empty()
            || self.label.len() > 128
            || self.label.chars().any(char::is_control)
            || self.label.eq_ignore_ascii_case("local")
        {
            return Err(invalid(
                "machine label must be 1–128 bytes, without controls, and cannot be Local",
            ));
        }
        if self.target.is_empty()
            || self.target.len() > 1024
            || self.target.starts_with('-')
            || self.target.chars().any(char::is_whitespace)
            || self.target.chars().any(char::is_control)
        {
            return Err(invalid(
                "invalid SSH target; use an SSH alias without spaces",
            ));
        }
        let authority = self.target.strip_prefix("ssh://").unwrap_or(&self.target);
        if authority
            .rsplit_once('@')
            .is_some_and(|(user, _)| user.contains(':'))
        {
            return Err(invalid("SSH targets cannot contain passwords"));
        }
        crate::session::validate_name(&self.session)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    version: u32,
    pub machines: Vec<Machine>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            version: 1,
            machines: Vec::new(),
        }
    }
}

impl Catalog {
    pub fn path() -> PathBuf {
        crate::config::config_path().with_file_name("ssh-machines.json")
    }

    pub fn load() -> io::Result<Self> {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> io::Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_CATALOG_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(io::Error::other("SSH machine catalog is too large"));
        }
        let catalog: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        catalog.validate()?;
        Ok(catalog)
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != 1 || self.machines.len() > 64 {
            return Err(io::Error::other(
                "unsupported SSH machine catalog version or size",
            ));
        }
        let mut ids = std::collections::HashSet::new();
        let mut labels = std::collections::HashSet::new();
        for machine in &self.machines {
            machine.validate()?;
            if !ids.insert(&machine.id) || !labels.insert(&machine.label) {
                return Err(io::Error::other("duplicate machine ID or label"));
            }
        }
        Ok(())
    }

    pub fn find(&self, selector: &str) -> io::Result<&Machine> {
        self.machines
            .iter()
            .find(|m| m.id == selector || m.label == selector)
            .ok_or_else(|| io::Error::other(format!("machine not found: {selector}")))
    }

    pub fn update(change: impl FnOnce(&mut Self) -> io::Result<()>) -> io::Result<()> {
        Self::update_at(&Self::path(), change)
    }

    fn update_at(path: &Path, change: impl FnOnce(&mut Self) -> io::Result<()>) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("catalog has no parent"))?;
        fs::create_dir_all(parent)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path.with_extension("lock"))?;
        // The descriptor stays open until after rename, serializing CLI mutations.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut catalog = Self::load_from(path)?;
        change(&mut catalog)?;
        catalog.validate()?;
        let bytes = serde_json::to_vec_pretty(&catalog).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(io::Error::other("catalog is too large"));
        }
        let temp = parent.join(format!(".ssh-machines-{:032x}.tmp", rand::random::<u128>()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            File::open(parent)?.sync_all()
        })();
        let _ = fs::remove_file(temp);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        Machine::new(
            "VM 223".into(),
            "devops@100.124.102.37".into(),
            "motionharvest".into(),
        )
        .unwrap()
    }

    #[test]
    fn validates_untrusted_catalog_fields() {
        for target in [
            "-oProxyCommand=evil",
            "bad\nhost",
            "ssh://user:secret@host",
            "VM 223",
            "",
        ] {
            let mut m = machine();
            m.target = target.into();
            assert!(m.validate().is_err(), "{target}");
        }
        for session in ["../default", "bad\nname", ".."] {
            let mut m = machine();
            m.session = session.into();
            assert!(m.validate().is_err(), "{session}");
        }
    }

    #[test]
    fn atomic_updates_preserve_profiles_and_reject_duplicates_and_corruption() {
        let dir =
            std::env::temp_dir().join(format!("herdr-catalog-{:032x}", rand::random::<u128>()));
        let path = dir.join("ssh-machines.json");
        let m = machine();
        Catalog::update_at(&path, |c| {
            c.machines.push(m.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            Catalog::load_from(&path).unwrap().find("VM 223").unwrap(),
            &m
        );
        let saved = fs::read(&path).unwrap();
        assert!(Catalog::update_at(&path, |c| {
            c.machines.push(m);
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), saved);
        fs::write(&path, "invalid json").unwrap();
        assert!(Catalog::update_at(&path, |_| Ok(())).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid json");
        fs::remove_dir_all(dir).unwrap();
    }
}
