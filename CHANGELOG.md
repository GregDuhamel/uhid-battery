# Changelog

All notable changes to this crate are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions are the git
tags consumers pin (`tag = "vX.Y.Z"`).

## [Unreleased]

### Changed

- **Breaking:** `Handle::inherited` and `Handle::inherited_with_others` are now
  `unsafe fn`. They always removed the `LISTEN_*` variables through
  `std::env::remove_var`, which is a data race against any thread reading the
  environment; the signature now says so, with a `# Safety` section carrying
  the precondition (call them at the top of `main`, before any thread exists).
- `Identity::uniq` is restricted to 1 to 63 characters from `[A-Za-z0-9._-]`
  (it was "not empty, no `/`"), and `Identity::name` may no longer be empty.
  `razerd`, `headset-<vvvv>-<pppp>` and the live tests' IDs are unaffected.
- `Battery::serve_until` services the device once more before returning for a
  readable `wake` descriptor or an interrupting signal, so a `GET_REPORT` is
  never left pending while the caller does its own thing.
- `Battery::serve_until` returns an error when `wake` is hung up, in error or
  not an open descriptor (`POLLHUP`/`POLLERR`/`POLLNVAL` without `POLLIN`).
  Such a descriptor is reported by every `poll()` and never becomes readable,
  so returning `true` for it had the caller spin.
- `Battery::wait_for_power_supply` keeps servicing the device between two looks
  at sysfs instead of sleeping, so the kernel's first `GET_REPORT` is answered
  while the probe runs. The timeout semantics are unchanged.
- `Handle::inherited*` stop at the first `LISTEN_FDS` number that is not an open
  descriptor, as `sd_listen_fds(3)` does, instead of carrying on with names that
  no longer line up; remove the `LISTEN_*` variables whenever they are addressed
  to this process, including `LISTEN_FDS=0`; and `fstat` each descriptor once
  instead of twice.
- Writes to `/dev/uhid` are retried on `EINTR` (`uhid_char_write` takes its
  lock with `mutex_lock_interruptible`).
- `Battery::serve_until` polls from a fixed two-slot array instead of a `Vec`
  allocated on every turn.

### Added

- `Cargo.lock` is committed, as cargo recommends for any crate that is built
  on its own, and the audit workflow reads it instead of regenerating one.
- This changelog.

### Removed

- `HeadsetControl` and `OpenRazer` from `doc-valid-idents` in `clippy.toml`:
  neither name appears in the crate.

## [0.2.0] - 2026-09-21

### Changed

- `Battery::create` refuses an `Identity` the kernel would truncate, or a
  `uniq` that is empty or holds a `/`: a truncated `uniq` names a power supply
  that is never found.
- `Handle::inherited` bounds `LISTEN_FDS` as `sd_listen_fds(3)` does and no
  longer closes numbers that are not open descriptors.
- `find_power_supply` only accepts a report ID after the name
  (`hid-<uniq>-battery[-<n>]`).
- `Kind::udev_rule` refuses a `phys` that would break out of its quotes.
- A `wait_for_power_supply` timeout too long for an `Instant` no longer panics.

### Added

- `Handle::inherited_with_others`, for daemons that are also handed a socket:
  the descriptors that were not adopted come back with their names.
- Workflows pin actions to commits, with Dependabot moving the pins (for cargo
  and for actions); the release bump input reaches the shell through the
  environment.

## [0.1.2] - 2026-09-21

- Release through a pull request now that `main` requires one.

## [0.1.1] - 2026-09-20

- Keep the tag named in the README in step with releases; require rustix 1.1.5
  or later.

## [0.1.0] - 2026-09-20

- Publish a battery to UPower through a virtual HID device: `Handle`,
  `Battery`, `Identity`, `Kind`, `find_power_supply`.

[Unreleased]: https://github.com/GregDuhamel/uhid-battery/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/GregDuhamel/uhid-battery/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/GregDuhamel/uhid-battery/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/GregDuhamel/uhid-battery/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/GregDuhamel/uhid-battery/releases/tag/v0.1.0
