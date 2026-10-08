//! The descriptors a service manager passed down, `sd_listen_fds(3)` style.
//!
//! systemd (and anything speaking the same protocol) hands a service open
//! descriptors by leaving them at numbers `3..3 + n` and announcing them in
//! the environment: `LISTEN_PID` names the process they are meant for,
//! `LISTEN_FDS` how many there are, and `LISTEN_FDNAMES` what each one is,
//! colon-separated and in order. A `OpenFile=/dev/uhid:uhid` line in a unit is
//! how a daemon of this crate gets `/dev/uhid` without being root; a
//! `ListenStream=` socket travels the same way.
//!
//! Nothing here is specific to uhid: [`take`] returns every descriptor with
//! its name, and [`crate::Handle::inherited`] is a filter over it.

use std::env;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};

use rustix::io::{FdFlags, fcntl_setfd};

/// Number of the first passed descriptor (`SD_LISTEN_FDS_START`): 0, 1 and 2
/// are the standard streams.
pub const LISTEN_FDS_START: RawFd = 3;

/// Takes the descriptors passed to this process, each with the name it was
/// passed under (empty when `LISTEN_FDNAMES` did not go that far).
///
/// Returns an empty vector when the process was not started that way, or when
/// the variables are addressed to a parent (`LISTEN_PID` is another process),
/// in which case they are left alone for it. Otherwise the `LISTEN_*`
/// variables are removed, even when they announce no descriptor
/// (`LISTEN_FDS=0`), so that neither a second call nor a child process claims
/// the same descriptors.
///
/// Each descriptor is marked close-on-exec on the way, as `sd_listen_fds(3)`
/// does with `unset_environment`. The protocol says nothing about what the
/// descriptors are: check before using one - [`crate::Handle::from_fd`] does
/// for `/dev/uhid`.
///
/// The walk stops at the first number that is not an open descriptor, again as
/// `sd_listen_fds(3)` does: the environment lied, the numbers after it are no
/// more trustworthy, and the names no longer line up with anything.
///
/// # Safety
///
/// The variables are removed with [`std::env::remove_var`], which is a data
/// race against any other thread that reads or writes the environment -
/// through `std::env`, through `getenv(3)` in a C library, or by a crate doing
/// either. The caller guarantees that no such thread exists while this runs,
/// which in practice means calling it at the top of `main`, before anything is
/// spawned.
#[allow(unsafe_code)] // Honest about `remove_var`; see the Safety section.
#[must_use]
pub unsafe fn take() -> Vec<(String, OwnedFd)> {
    let Some(count) = announced(
        env::var("LISTEN_PID").ok().as_deref(),
        env::var("LISTEN_FDS").ok().as_deref(),
        std::process::id(),
    ) else {
        return Vec::new();
    };
    let names = env::var("LISTEN_FDNAMES").unwrap_or_default();
    let mut names = names.split(':');

    let mut passed = Vec::new();
    for offset in 0..count {
        let name = names.next().unwrap_or_default();
        // SAFETY: by the sd_listen_fds protocol the descriptors in
        // `LISTEN_FDS_START..LISTEN_FDS_START + count` belong to this process,
        // this is the only place that adopts them, and the variables are
        // cleared below so nothing adopts them again.
        let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START + offset) };
        // Passed descriptors come without FD_CLOEXEC. A failure means the
        // number is not an open descriptor: let go of it without closing what
        // is not ours, and stop there.
        if fcntl_setfd(&fd, FdFlags::CLOEXEC).is_err() {
            let _ = fd.into_raw_fd();
            break;
        }
        passed.push((name.to_owned(), fd));
    }

    // SAFETY: the caller guarantees no thread touches the environment.
    unsafe { unset() };
    passed
}

/// How many descriptors `LISTEN_FDS` announces to the process `pid`, given
/// the two variables. `None` when they are absent or meant for another process
/// - and then they are not ours to remove either.
///
/// A missing or unparsable count, or one past the bounds `sd_listen_fds(3)`
/// applies (the last descriptor number has to be one a descriptor can have),
/// announces nothing - `Some(0)` - but the variables still get removed, as
/// they would for a plain `LISTEN_FDS=0`.
fn announced(listen_pid: Option<&str>, listen_fds: Option<&str>, pid: u32) -> Option<RawFd> {
    let addressed: u32 = listen_pid?.parse().ok()?;
    if addressed != pid {
        return None;
    }
    let count = listen_fds
        .and_then(|count| count.parse::<RawFd>().ok())
        .filter(|&count| count > 0 && count <= RawFd::MAX - LISTEN_FDS_START)
        .unwrap_or(0);
    Some(count)
}

/// Removes the `LISTEN_*` variables.
///
/// # Safety
///
/// No other thread may read or write the environment while this runs; see
/// [`take`].
#[allow(unsafe_code)] // The only place that edits the environment.
unsafe fn unset() {
    for key in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: the caller guarantees the environment is not being read.
        unsafe {
            env::remove_var(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parsing is tested on its own: the unit tests run on several
    /// threads, and editing the environment under them is the very race the
    /// `# Safety` section forbids.
    #[test]
    fn the_variables_are_read_as_sd_listen_fds_does() {
        assert_eq!(announced(Some("42"), Some("2"), 42), Some(2));
        assert_eq!(announced(Some("42"), Some("1"), 42), Some(1));
        // Addressed to someone else, or to nobody: not ours.
        assert_eq!(announced(Some("41"), Some("2"), 42), None);
        assert_eq!(announced(None, Some("2"), 42), None);
        assert_eq!(announced(Some("pid"), Some("2"), 42), None);
        // Ours, but announcing nothing - the variables are still removed.
        assert_eq!(announced(Some("42"), Some("0"), 42), Some(0));
        assert_eq!(announced(Some("42"), None, 42), Some(0));
        assert_eq!(announced(Some("42"), Some("many"), 42), Some(0));
        assert_eq!(announced(Some("42"), Some("-1"), 42), Some(0));
        // `3 + count - 1` has to fit a descriptor number.
        let limit = RawFd::MAX - LISTEN_FDS_START;
        assert_eq!(
            announced(Some("42"), Some(&limit.to_string()), 42),
            Some(limit)
        );
        assert_eq!(
            announced(Some("42"), Some(&(limit + 1).to_string()), 42),
            Some(0)
        );
    }

    #[test]
    #[allow(unsafe_code)] // Calling `take` where it cannot edit anything.
    fn nothing_is_taken_when_nothing_was_passed() {
        // The variables are not set under `cargo test`, so this is the
        // not-started-that-way path: it reads the environment and nothing
        // else, which is why it is safe to call from a test thread.
        // SAFETY: the variables are absent, so no removal happens.
        let passed = unsafe { take() };
        assert_eq!(passed.len(), 0);
    }
}
