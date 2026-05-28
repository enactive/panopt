//! Ensuring the panopt daemon is running.

use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::paths;

/// True if something is accepting connections on `127.0.0.1:port`.
fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Locate the `panoptd` binary: prefer one next to this executable (a cargo
/// build, or a co-installed pair), otherwise trust `PATH`.
fn panoptd_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|d| d.join("panoptd")) {
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("panoptd")
}

/// Ensure `panoptd` is listening on `port`, starting it detached if not.
///
/// A freshly started daemon is placed in its own session (`setsid`) so it
/// outlives the launching terminal and any Zellij session: it is the global
/// daemon for every project, never tied to one cockpit.
///
/// `host` is forwarded as `panoptd --host <addr>` and selects the bind
/// address: `None` keeps the daemon default (`127.0.0.1`); pass
/// `Some("0.0.0.0")` to expose the daemon to other machines. The startup
/// probe always checks loopback - `0.0.0.0` covers it, and a specific-
/// interface bind that excludes loopback is exotic enough that the caller
/// should start `panoptd` by hand.
pub fn ensure(host: Option<&str>, port: u16) -> Result<()> {
    if port_open(port) {
        return Ok(());
    }

    let log = paths::daemon_log()?;
    let out = std::fs::File::create(&log)
        .with_context(|| format!("creating daemon log {}", log.display()))?;
    let errlog = out.try_clone()?;

    let bind = host.unwrap_or("127.0.0.1");
    eprintln!("panopt: starting panoptd on {bind}:{port}");
    let mut cmd = Command::new(panoptd_path());
    cmd.arg("--port").arg(port.to_string());
    if let Some(h) = host {
        cmd.arg("--host").arg(h);
    }
    cmd.stdin(Stdio::null()).stdout(out).stderr(errlog);
    // SAFETY: `setsid` is async-signal-safe, which is the contract a `pre_exec`
    // hook must honour - it runs in the child between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
        .context("starting panoptd (is the binary next to panopt, or on PATH?)")?;

    for _ in 0..50 {
        if port_open(port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "panoptd did not start listening on 127.0.0.1:{port} within 5s; see {}",
        log.display()
    )
}
