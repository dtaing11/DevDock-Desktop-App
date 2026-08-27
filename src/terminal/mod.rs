//! Integrated terminal: a pty running a shell, and enough of a terminal
//! emulator to show what it prints.

pub mod vt;

#[cfg(unix)]
pub mod pty;

/// Whether this platform has a terminal at all.
pub const SUPPORTED: bool = cfg!(unix);
