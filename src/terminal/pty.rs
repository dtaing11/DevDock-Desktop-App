//! A pseudo-terminal running a shell.
//!
//! Unix only. A pty is how a terminal works: the child believes it is
//! attached to a real terminal, so the shell shows a prompt, `ls` colourises
//! its output, and `^C` reaches the foreground process group. Piping
//! stdin/stdout instead would give a command runner, not a terminal.
//!
//! Windows needs ConPTY, which is a different API; the terminal is simply
//! unavailable there rather than pretending with pipes.

use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::vt::Screen;

/// A running shell, its screen, and the pty connecting them.
pub struct Pty {
    master: RawFd,
    pid: libc::pid_t,
    pub screen: Arc<Mutex<Screen>>,
    alive: Arc<AtomicBool>,
    /// What the child was started with, for the UI.
    pub command: String,
}

impl Pty {
    /// Starts `command` (a shell) in `cwd` on a new pty.
    ///
    /// `on_output` is called whenever the screen changes, so a GUI can
    /// repaint without polling.
    pub fn spawn(
        command: &str,
        cwd: &Path,
        cols: u16,
        rows: u16,
        on_output: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, String> {
        let screen = Arc::new(Mutex::new(Screen::new(cols as usize, rows as usize)));
        let alive = Arc::new(AtomicBool::new(true));

        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: openpty fills the two fds; every pointer is valid and
        // owned by this frame.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if opened != 0 {
            return Err(format!(
                "cannot open a pty: {}",
                std::io::Error::last_os_error()
            ));
        }

        // SAFETY: fork duplicates this process; the child only calls
        // async-signal-safe functions before exec.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(format!("cannot fork: {}", std::io::Error::last_os_error()));
        }

        if pid == 0 {
            // Child: become a session leader with the slave as its
            // controlling terminal, then exec the shell.
            unsafe {
                libc::close(master);
                libc::setsid();
                libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
                libc::dup2(slave, 0);
                libc::dup2(slave, 1);
                libc::dup2(slave, 2);
                if slave > 2 {
                    libc::close(slave);
                }
                let cwd = std::ffi::CString::new(cwd.as_os_str().to_string_lossy().as_bytes())
                    .unwrap_or_default();
                libc::chdir(cwd.as_ptr());

                // A terminal that says it is dumb gets no colours; one that
                // claims too much gets sequences we do not implement.
                let term = std::ffi::CString::new("TERM=xterm-256color").unwrap();
                libc::putenv(term.into_raw());

                let program = std::ffi::CString::new(command).unwrap_or_default();
                let argv = [program.as_ptr(), std::ptr::null()];
                libc::execvp(program.as_ptr(), argv.as_ptr());
                // exec only returns on failure, and this is a forked child:
                // it must not unwind back into the parent's code.
                libc::_exit(127);
            }
        }

        // Parent.
        unsafe { libc::close(slave) };

        // The reader owns a dup of the master so the screen keeps filling
        // even while the writer holds the original.
        let read_fd = unsafe { libc::dup(master) };
        let reader_screen = screen.clone();
        let reader_alive = alive.clone();
        std::thread::spawn(move || {
            // SAFETY: read_fd is ours, and File takes ownership of it.
            let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let mut buffer = [0u8; 8192];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut screen) = reader_screen.lock() {
                            screen.feed(&buffer[..n]);
                        }
                        if let Some(notify) = &on_output {
                            notify();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            reader_alive.store(false, Ordering::SeqCst);
            if let Some(notify) = &on_output {
                notify();
            }
        });

        Ok(Self { master, pid, screen, alive, command: command.to_string() })
    }

    /// Whether the shell is still running.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Sends input to the shell.
    pub fn write(&self, bytes: &[u8]) {
        // SAFETY: the fd is ours and stays open for the lifetime of `self`;
        // ManuallyDrop keeps File from closing it.
        let mut file = std::mem::ManuallyDrop::new(unsafe {
            std::fs::File::from_raw_fd(self.master)
        });
        let _ = file.write_all(bytes);
        let _ = file.flush();
    }

    /// Tells the child the window changed size.
    ///
    /// Without this the shell keeps wrapping at the old width, which is the
    /// most visible way a home-made terminal looks broken.
    pub fn resize(&self, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the fd is ours and the struct outlives the call.
        unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ as _, &size) };
        if let Ok(mut screen) = self.screen.lock() {
            screen.resize(cols as usize, rows as usize);
        }
    }

    /// Sends a signal to whatever is in the foreground, the way a real
    /// terminal does when you press ^C.
    ///
    /// The foreground process group, not the shell's: `sh` runs each command
    /// it starts in a process group of its own, so signalling the shell's
    /// group leaves the running command untouched and interrupts nothing. The
    /// tty knows which group is in front — that is what `tcgetpgrp` is for,
    /// and it is what the kernel's own line discipline uses to deliver ^C.
    pub fn signal(&self, signal: i32) {
        // SAFETY: the fd is ours; the group is this tty's own foreground.
        let group = unsafe { libc::tcgetpgrp(self.master) };
        let group = if group > 0 { group } else { self.pid };
        // SAFETY: killing our own child's group.
        unsafe { libc::killpg(group, signal) };
    }

    /// The process group currently in the foreground of this terminal, if the
    /// tty will say.
    fn foreground(&self) -> Option<libc::pid_t> {
        // SAFETY: the fd is ours.
        let group = unsafe { libc::tcgetpgrp(self.master) };
        (group > 0).then_some(group)
    }

    /// The screen's text, for tests and for copying out.
    pub fn text(&self) -> String {
        self.screen.lock().map(|s| s.text()).unwrap_or_default()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // A terminal whose panel is closed must not leave a shell running —
        // nor whatever the shell was running. That is a separate process
        // group, so it needs signalling separately: killing only the shell
        // orphans a build, a server, or a `tail -f` with nowhere to write.
        let foreground = self.foreground().filter(|g| *g != self.pid);
        unsafe {
            if let Some(group) = foreground {
                libc::killpg(group, libc::SIGHUP);
                libc::killpg(group, libc::SIGKILL);
            }
            libc::killpg(self.pid, libc::SIGHUP);
            libc::kill(self.pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(self.pid, &mut status, libc::WNOHANG);
            libc::close(self.master);
        }
    }
}

/// The shell to start: the user's, or a sensible fallback.
pub fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}
