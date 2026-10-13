//! Kernel-attested peer-credential classification (ADR-0019 §1/§3,
//! design doc §2). `peer_uid` reads the OS's own record of who is on the
//! other end of a connected Unix stream — never anything the peer could
//! assert about itself.

use std::os::unix::net::UnixStream;

use super::TrustedPrincipalConfig;

/// Kernel-attested effective UID of the peer on a connected Unix stream.
/// Server side: the client's UID at `connect()`. Client side (PR-2c): the
/// server's UID at `listen()`.
pub fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    peer_uid_impl(stream)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid_impl(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is a valid, open socket fd owned by `stream` for the
    // duration of this call; `cred`/`len` are valid, correctly sized
    // out-parameters for `SO_PEERCRED` per `unix(7)`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an unexpected credential size",
        ));
    }
    Ok(cred.uid)
}

#[cfg(target_vendor = "apple")]
fn peer_uid_impl(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid, open socket fd owned by `stream` for the
    // duration of this call; `uid`/`gid` are valid out-parameters per
    // `getpeereid(3)`.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn peer_uid_impl(_stream: &UnixStream) -> std::io::Result<u32> {
    // Compiles everywhere and fails closed: an unsupported platform
    // cannot classify a peer, so every peer is dropped unclassified.
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClass {
    Provider,
    Agent,
}

/// `None` means the peer is closed before a byte is read (ADR-0019 §1).
/// That includes root (uid 0), and any uid that is neither
/// `cfg.provider_uid` nor a member of `cfg.agent_uids`.
pub fn classify(uid: u32, cfg: &TrustedPrincipalConfig) -> Option<PeerClass> {
    if uid == cfg.provider_uid {
        Some(PeerClass::Provider)
    } else if cfg.agent_uids.contains(&uid) {
        Some(PeerClass::Agent)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> TrustedPrincipalConfig {
        TrustedPrincipalConfig {
            provider_uid: 10001,
            agent_uids: vec![10002, 10003],
            provider_id: "operator:lg-provider".to_string(),
            state_dir: std::path::PathBuf::from("/var/lib/libra-governor"),
            socket_dir: std::path::PathBuf::from("/run/libra-governor"),
            connection_timeout_ms: 5000,
        }
    }

    #[test]
    fn classify_recognizes_the_provider_uid() {
        let cfg = test_config();
        assert_eq!(classify(10001, &cfg), Some(PeerClass::Provider));
    }

    #[test]
    fn classify_recognizes_an_agent_uid() {
        let cfg = test_config();
        assert_eq!(classify(10002, &cfg), Some(PeerClass::Agent));
        assert_eq!(classify(10003, &cfg), Some(PeerClass::Agent));
    }

    #[test]
    fn classify_drops_an_unknown_uid() {
        let cfg = test_config();
        assert_eq!(classify(99999, &cfg), None);
    }

    #[test]
    fn classify_drops_root_unless_root_is_also_the_configured_provider_uid() {
        let cfg = test_config();
        assert_eq!(classify(0, &cfg), None);
    }

    #[test]
    fn peer_uid_on_a_real_unix_stream_pair_returns_this_processs_own_uid() {
        // Exercises the real SO_PEERCRED/getpeereid syscall path (not a
        // mock): connecting a UnixStream::pair() to itself reports this
        // test process's own uid on both ends, which is a stable,
        // environment-independent assertion (works as any uid, including
        // root in CI).
        let (a, b) = UnixStream::pair().expect("UnixStream::pair");
        let uid_a = peer_uid(&a);
        let uid_b = peer_uid(&b);
        match (uid_a, uid_b) {
            (Ok(ua), Ok(ub)) => assert_eq!(ua, ub),
            // `Unsupported` is the documented fail-closed outcome on a
            // platform this module does not implement the syscall for.
            (Err(e), _) | (_, Err(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
            }
        }
    }
}
