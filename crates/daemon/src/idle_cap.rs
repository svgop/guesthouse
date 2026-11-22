//! The idle cap: a request that keeps the deep sleep states off the host's cores.
//!
//! A guest's vCPU thread wakes from a halt on whichever core it last ran on, and waking
//! a core out of a deep C-state costs its exit latency before the first guest instruction
//! runs. A request written to `/dev/cpu_dma_latency` asks the whole host to wake within a
//! given latency: the kernel admits only idle states whose exit latency is strictly under
//! the value, and the request lives exactly as long as the descriptor that wrote it. The
//! node is read as a signed 32-bit number, so a value of 2^31 or above arrives negative and
//! is dropped without a word, which is why the cap takes an `i32`.
//!
//! The cap is taken when QEMU first connects and goes when QEMU has gone, so it covers the
//! machine's life and nothing else: a daemon that is waiting for a VM, or one left behind
//! by an app that crashed, holds nothing. The kernel closes the request when the process
//! goes, crash or kill included, which is the rest of the reversibility story.
//!
//! The value is microseconds, the host's own unit. Asking for a deep state's exact exit
//! latency keeps that state: only a value strictly under it removes it. A cap of 300 on a
//! host whose deepest state exits in 350 excludes it and leaves the shallow states, each
//! of which costs an order of magnitude less to leave, in play.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The node the kernel's idle governor listens to.
pub const NODE: &str = "/dev/cpu_dma_latency";

/// The node, opened and not yet written: a cap waiting for a machine to run.
#[derive(Debug)]
pub struct IdleCap {
    node: File,
}

impl IdleCap {
    /// Opens `node` for a request that is written later, so a daemon can hold the right to
    /// ask before it has anything to ask for. The descriptor is close-on-exec by necessity:
    /// starting a backend runs helper programs, and a child that inherited the descriptor
    /// would keep the host capped after the daemon itself let go.
    pub fn open(node: &Path) -> io::Result<Self> {
        let node = OpenOptions::new().write(true).custom_flags(libc::O_CLOEXEC).open(node)?;
        Ok(Self { node })
    }

    /// Asks the host to wake any core within `micros`, from now until the cap goes. Writing
    /// again replaces the request, so the same descriptor serves a cap that is taken up and
    /// put down as machines come and go.
    pub fn ask(&mut self, micros: i32) -> io::Result<()> {
        self.node.write_all(&micros.to_le_bytes())
    }

    /// Whether the request would die at an exec, which the NVIDIA start depends on: it runs
    /// a helper program between the open and the jail.
    pub fn closes_on_exec(&self) -> io::Result<bool> {
        // SAFETY: F_GETFD on a descriptor this process owns reads a flag and nothing else.
        let flags = unsafe { libc::fcntl(self.node.as_raw_fd(), libc::F_GETFD) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags & libc::FD_CLOEXEC != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A regular file stands in for the node: the open, the four-byte request, and the
    /// close-on-exec flag are all checkable there; only the governor's answer needs the
    /// real node, and that is the live arm's measurement.
    fn stand_in(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("gh-idle-cap-{}-{name}", std::process::id()));
        fs::write(&path, []).unwrap();
        path
    }

    #[test]
    fn an_opened_node_holds_nothing_until_asked() {
        let node = stand_in("deferred");
        let mut cap = IdleCap::open(&node).unwrap();
        assert!(fs::read(&node).unwrap().is_empty(), "the open alone must not ask for anything");
        cap.ask(300).unwrap();
        assert_eq!(fs::read(&node).unwrap(), 300i32.to_le_bytes());
        drop(cap);
        fs::remove_file(&node).unwrap();
    }

    /// The node keeps one request per descriptor and a write replaces it, so a cap that is asked
    /// for again never leaves two requests behind. On a file the second write lands where the
    /// first ended, which the node does not do, so what is checked here is what both share:
    /// every ask is exactly the four bytes the node reads, on the one descriptor the cap owns.
    #[test]
    fn every_ask_is_one_four_byte_request_on_the_one_descriptor() {
        let node = stand_in("rewrite");
        let mut cap = IdleCap::open(&node).unwrap();
        cap.ask(300).unwrap();
        assert_eq!(fs::read(&node).unwrap(), 300i32.to_le_bytes());
        cap.ask(0).unwrap();
        assert_eq!(fs::metadata(&node).unwrap().len(), 8, "two asks, two four-byte requests, one descriptor");
        assert_eq!(fs::read(&node).unwrap()[4..], 0i32.to_le_bytes());
        drop(cap);
        fs::remove_file(&node).unwrap();
    }

    #[test]
    fn a_node_that_is_not_there_is_an_error_not_a_panic() {
        let missing = std::env::temp_dir().join(format!("gh-idle-cap-missing-{}", std::process::id()));
        assert_eq!(IdleCap::open(&missing).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_cap_dies_at_an_exec() {
        let node = stand_in("cloexec");
        let mut cap = IdleCap::open(&node).unwrap();
        cap.ask(300).unwrap();
        assert!(cap.closes_on_exec().unwrap(), "a helper child must not inherit the request");
        // The flag is read by a child, which is what an exec is: the descriptor must not be
        // among the child's.
        let held = std::process::Command::new("sh")
            .args(["-c", "ls /proc/$$/fd | grep -c '^3$'"])
            .stdout(std::process::Stdio::piped())
            .output()
            .unwrap();
        let seen = String::from_utf8_lossy(&held.stdout).trim().to_owned();
        assert_eq!(seen, "0", "the child inherited the request's descriptor");
        drop(cap);
        fs::remove_file(&node).unwrap();
    }
}
