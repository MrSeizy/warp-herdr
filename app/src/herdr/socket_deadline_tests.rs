#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;

#[cfg(unix)]
use super::*;

#[cfg(unix)]
fn is_nonblocking(stream: &UnixStream) -> bool {
    // SAFETY: F_GETFL only reads the status flags for this valid, borrowed fd.
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    assert!(
        flags >= 0,
        "F_GETFL failed: {}",
        std::io::Error::last_os_error()
    );
    flags & libc::O_NONBLOCK != 0
}

#[cfg(unix)]
#[test]
fn ready_data_cannot_bypass_deadline_and_blocking_is_restored() {
    let (mut peer, stream) = UnixStream::pair().unwrap();
    peer.write_all(b"continuously readable data\n").unwrap();

    let err = read_line_with_deadline(&stream, Instant::now()).unwrap_err();

    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(!is_nonblocking(&stream));
}

#[cfg(unix)]
#[test]
fn overlong_line_is_rejected_and_blocking_is_restored() {
    let (mut peer, stream) = UnixStream::pair().unwrap();
    peer.write_all(b"abcde\n").unwrap();

    let err = read_line_with_deadline_limit(&stream, Instant::now() + Duration::from_secs(1), 4)
        .unwrap_err();

    assert!(err.to_string().contains("exceeded 4 bytes"), "{err}");
    assert!(!is_nonblocking(&stream));
}
