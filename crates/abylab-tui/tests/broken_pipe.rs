#![cfg(unix)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};

/// Exercise the real entry point: the unit-test harness does not run main(),
/// so it cannot detect main resetting Rust's default SIGPIPE disposition.
#[test]
fn closed_diagnostic_pipe_does_not_kill_abylab() {
    let mut fds = [-1; 2];
    // SAFETY: fds has space for both descriptors returned by pipe().
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: pipe succeeded and these descriptors each have exactly one owner.
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // Close the reader before spawning so the first diagnostic write fails
    // deterministically, without racing the child's startup.
    drop(reader);

    let status = Command::new(env!("CARGO_BIN_EXE_abylab"))
        .arg("--invalid-broken-pipe-test-argument")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(writer))
        .status()
        .expect("run abylab with a closed diagnostic pipe");

    assert_eq!(
        status.code(),
        Some(1),
        "the argument error should exit normally, not terminate on SIGPIPE: {status}"
    );
}
