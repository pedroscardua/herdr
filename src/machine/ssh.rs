//! SSH transport and explicit setup. Reconnects never install or stop a server.
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use super::Machine;

// Never replace the official ~/.local/bin/herdr, or use its default session.
const REMOTE_BINARY: &str = "\"$HOME/.local/lib/herdr-motionharvest/herdr\"";

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command(target: &str, interactive: bool) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    if !interactive {
        cmd.args(["-o", "BatchMode=yes"]);
    }
    cmd.arg(target);
    cmd
}

fn checked(output: Output) -> io::Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(io::Error::other(format!(
            "SSH command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub(crate) fn check(machine: &Machine, interactive: bool) -> io::Result<u32> {
    machine.validate()?;
    let output = command(&machine.target, interactive)
        .arg(format!("{REMOTE_BINARY} status client --json"))
        .output()?;
    let actual: serde_json::Value =
        serde_json::from_str(&checked(output)?).map_err(io::Error::other)?;
    let expected = env!("CARGO_PKG_VERSION");
    if actual["version"].as_str() != Some(expected) {
        return Err(io::Error::other(format!(
            "expected {expected} in the isolated motionharvest install, got {actual}"
        )));
    }
    match actual["protocol"].as_u64() {
        Some(protocol @ (12 | 13)) => Ok(protocol as u32),
        _ => Err(io::Error::other(
            "unsupported isolated motionharvest protocol",
        )),
    }
}

pub(crate) fn setup(machine: &Machine, install: bool, binary: Option<&Path>) -> io::Result<u32> {
    if binary.is_none() {
        if let Ok(protocol) = check(machine, true) {
            return Ok(protocol);
        }
    }
    if !install {
        return Err(io::Error::other("the isolated motionharvest binary is missing or SSH failed; verify SSH, then rerun machine add with --install (optionally --binary /path/to/remote-platform/herdr)"));
    }
    let platform = checked(
        command(&machine.target, true)
            .arg("uname -s; uname -m")
            .output()?,
    )?;
    let mut parts = platform.lines();
    let os = match parts.next() {
        Some("Linux") => "linux",
        Some("Darwin") => "macos",
        _ => {
            return Err(io::Error::other(
                "only Linux and macOS SSH hosts are supported",
            ))
        }
    };
    let arch = match parts.next() {
        Some("x86_64" | "amd64") => "x86_64",
        Some("aarch64" | "arm64") => "aarch64",
        _ => {
            return Err(io::Error::other(
                "only x86_64 and aarch64 SSH hosts are supported",
            ))
        }
    };
    if let Some(binary) = binary {
        upload(machine, binary)?;
    } else {
        download_and_upload(machine, &format!("herdr-{os}-{arch}"))?;
    }
    check(machine, true)
}

#[derive(serde::Deserialize)]
struct Release {
    assets: Vec<Asset>,
}
#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

fn curl(url: &str) -> Command {
    let mut cmd = Command::new("curl");
    cmd.args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "10",
        "--max-time",
        "120",
        url,
    ]);
    cmd
}

fn download_and_upload(machine: &Machine, name: &str) -> io::Result<()> {
    let url = format!(
        "https://api.github.com/repos/motionharvest/herdr/releases/tags/v{}",
        env!("CARGO_PKG_VERSION")
    );
    let output = curl(&url).output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "could not read the motionharvest GitHub release; use --binary for a custom build",
        ));
    }
    let release: Release = serde_json::from_slice(&output.stdout).map_err(io::Error::other)?;
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| {
            io::Error::other(
                "remote-platform asset is not available in the motionharvest release; use --binary",
            )
        })?;
    let digest = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .ok_or_else(|| io::Error::other("release asset has no SHA-256 digest; use --binary"))?;
    let prefix = "https://github.com/motionharvest/herdr/releases/download/";
    if !asset.browser_download_url.starts_with(prefix) {
        return Err(io::Error::other(
            "unexpected motionharvest release asset URL",
        ));
    }
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("herdr-machine-{:032x}", rand::random::<u128>()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let path = dir.join("herdr");
    let result = (|| {
        let status = curl(&asset.browser_download_url)
            .arg("--output")
            .arg(&path)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("remote binary download failed"));
        }
        crate::checksum::verify_sha256(&path, digest)?;
        upload(machine, &path)
    })();
    let _ = std::fs::remove_dir_all(dir);
    result
}

fn upload(machine: &Machine, path: &Path) -> io::Result<()> {
    let mut source = std::fs::File::open(path)?;
    // A unique staging name and EXIT trap leave the old binary intact on failure.
    // Replacing the file does not stop existing processes or their sessions.
    let script = format!(
        "set -eu; umask 077; mkdir -p \"$HOME/.local/lib/herdr-motionharvest\"; \
         tmp=$(mktemp \"$HOME/.local/lib/herdr-motionharvest/.install.XXXXXX\"); \
         trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; \
         cat > \"$tmp\"; chmod 700 \"$tmp\"; \
         test \"$(\"$tmp\" --version)\" = {}; mv -f \"$tmp\" {REMOTE_BINARY}",
        quote(&format!("herdr {}", env!("CARGO_PKG_VERSION")))
    );
    let mut child = command(&machine.target, true)
        .arg(format!("sh -c {}", quote(&script)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("SSH stdin unavailable"))?;
    let copy = io::copy(&mut source, &mut stdin);
    drop(stdin);
    let result = checked(child.wait_with_output()?);
    copy?;
    result.map(|_| ())
}

pub(crate) struct Tunnel {
    child: Child,
    shutdown: UnixStream,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Tunnel {
    pub fn error(&self) -> String {
        self.stderr
            .lock()
            .ok()
            .map(|s| String::from_utf8_lossy(&s).trim().to_owned())
            .unwrap_or_default()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private socket pair avoids filesystem sockets, collisions and stale bridges.
pub(crate) fn connect(machine: &Machine) -> io::Result<(UnixStream, Tunnel)> {
    machine.validate()?;
    let remote = format!(
        "export HERDR_CONFIG_PATH=\"$HOME/.config/herdr-motionharvest/config.toml\"; exec {REMOTE_BINARY} --session {} remote-client-bridge",
        quote(&machine.session)
    );
    let mut child = command(&machine.target, false)
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (client, bridge) = UnixStream::pair()?;
    let shutdown = bridge.try_clone()?;
    let mut upload = bridge.try_clone()?;
    let mut download = bridge;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing SSH stdin"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing SSH stdout"))?;
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut reader) = child.stderr.take() {
        let captured = stderr.clone();
        std::thread::spawn(move || {
            let mut bytes = [0; 1024];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                if let Ok(mut text) = captured.lock() {
                    // Bound errors even when a misconfigured remote prints forever.
                    let available = 8192usize.saturating_sub(text.len());
                    text.extend_from_slice(&bytes[..n.min(available)]);
                }
            }
        });
    }
    std::thread::spawn(move || {
        let _ = io::copy(&mut upload, &mut stdin);
    });
    std::thread::spawn(move || {
        let _ = io::copy(&mut stdout, &mut download);
        let _ = download.shutdown(std::net::Shutdown::Both);
    });
    Ok((
        client,
        Tunnel {
            child,
            shutdown,
            stderr,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_quotes_cannot_inject_commands() {
        assert_eq!(quote("a'b"), "'a'\\''b'");
        assert_eq!(quote("$(touch /tmp/test); x"), "'$(touch /tmp/test); x'");
    }
    #[test]
    fn transport_is_noninteractive_and_has_liveness_bounds() {
        let cmd = command("VM223", false);
        let args: Vec<_> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "BatchMode=yes"));
        assert!(args.iter().any(|a| a == "ConnectTimeout=10"));
        assert_eq!(args.last().unwrap(), "VM223");
    }
}
