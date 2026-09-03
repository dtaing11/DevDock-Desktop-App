//! Tests of the integrated terminal against a real shell on a real pty.
//!
//! The emulator is unit-tested next to its own code; what these check is
//! the part that only fails in the presence of an actual process: that the
//! shell starts, sees a terminal, answers input, and dies when told to.

#![cfg(unix)]

use git_manage::terminal::pty::Pty;
use git_manage::terminal::vt::Screen;
use std::time::{Duration, Instant};

/// Waits for the screen to satisfy `check`, so tests never race the shell.
///
/// The thing to be careful about is that a terminal echoes what you type, so
/// waiting for text that is *in the command* is satisfied the instant the
/// command is typed — before the shell has run it, and even if it never does.
/// Every command below is therefore written so its output differs from the
/// keystrokes that produced it, usually by putting part of the marker in a
/// variable the shell expands.
fn eventually(pty: &Pty, what: &str, check: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let text = pty.text();
        if check(&text) {
            return text;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}; screen was:\n{}", pty.text());
}

/// Waits on the screen itself, for when what is being tested is a cell's
/// styling rather than its text.
fn eventually_screen(pty: &Pty, what: &str, check: impl Fn(&Screen) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check(&pty.screen.lock().unwrap()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}; screen was:\n{}", pty.text());
}

#[test]
fn a_shell_runs_a_command_and_its_output_reaches_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).expect("pty");
    assert!(pty.alive());

    // `$what` is expanded by the shell, so the assembled marker can only
    // appear because the command actually ran.
    pty.write(b"what=pty; echo hello-from-the-$what\n");
    let text = eventually(&pty, "the echo output", |t| t.contains("hello-from-the-pty"));
    // The command is echoed by the tty and its output follows: both are on
    // the screen, which is what makes it a terminal rather than a pipe.
    assert!(
        text.contains("hello-from-the-$what"),
        "the tty did not echo the command:\n{text}"
    );
}

#[test]
fn the_child_starts_in_the_directory_it_was_given() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker-file.txt"), "x").unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    pty.write(b"ls\n");
    eventually(&pty, "the directory listing", |t| t.contains("marker-file.txt"));
}

#[test]
fn the_child_believes_it_is_attached_to_a_terminal() {
    // The whole reason for a pty: `test -t 1` is false through a pipe.
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    pty.write(b"kind=TTY; test -t 1 && echo IS-A-$kind\n");
    eventually(&pty, "the tty check", |t| t.contains("IS-A-TTY"));

    // And it is told how big the window is.
    pty.write(b"stty size\n");
    let text = eventually(&pty, "the window size", |t| t.contains("24 80"));
    assert!(text.contains("24 80"), "{text}");
}

#[test]
fn resizing_reaches_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();
    pty.write(b"go=ready; echo shell-$go\n");
    eventually(&pty, "the shell to start", |t| t.contains("shell-ready"));

    pty.resize(100, 30);
    pty.write(b"stty size\n");
    eventually(&pty, "the new window size", |t| t.contains("30 100"));
}

#[test]
fn a_signal_interrupts_the_foreground_program() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    // Start something that would never finish on its own.
    pty.write(b"sleep 60\n");
    std::thread::sleep(Duration::from_millis(400));
    pty.signal(libc::SIGINT);

    // ^C ends the sleep and the shell carries on. This *is* the assertion:
    // had the signal not reached the sleep, the shell would still be inside
    // it and nothing below would ever run. `$state` is expanded by the shell,
    // so the tty echoing the command cannot produce the marker on its own —
    // waiting for text that appears in the keystrokes would pass either way.
    pty.write(b"state=here; echo still-$state\n");
    eventually(&pty, "the shell after an interrupt", |t| t.contains("still-here"));
}

#[test]
fn colour_output_keeps_its_colour() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    // Waiting for the text would be satisfied by the tty echoing the command,
    // which contains "RED" as a literal. What is being waited for is the
    // colour, so that is what the wait looks at.
    pty.write(b"printf '\\033[31mRED\\033[0m\\n'\n");
    eventually_screen(&pty, "the coloured output", |screen: &Screen| {
        screen.lines().iter().any(|line| {
            line.iter().any(|cell| {
                cell.ch == 'R'
                    && cell.style.fg == Some(git_manage::terminal::vt::Color::Red)
            })
        })
    });
}

#[test]
fn exiting_the_shell_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();
    pty.write(b"exit\n");

    let deadline = Instant::now() + Duration::from_secs(5);
    while pty.alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!pty.alive(), "the terminal should notice its shell exiting");
}

#[test]
fn dropping_the_terminal_kills_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();
    pty.write(b"go=ready; echo shell-$go\n");
    eventually(&pty, "the shell to start", |t| t.contains("shell-ready"));

    // A closed panel must not leave a shell running.
    drop(pty);
}

/// Closing a panel must not orphan what the shell was running.
///
/// The command runs in a process group of its own, so nothing that only
/// signals the shell reaches it — a build, a dev server, or a `tail -f` would
/// be left going with nowhere to write and no window to stop it from.
#[test]
fn dropping_the_terminal_kills_what_the_shell_was_running() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    // The subshell records its own pid and then *becomes* the sleep, so the
    // file holds the pid of the process actually in the foreground. Asking
    // about one pid beats searching the process table for a marker: anything
    // else on the machine that happens to mention it — an editor, a grep, the
    // command that wrote this test — would answer for it.
    pty.write(b"sh -c 'echo $$ > child.pid; exec sleep 60'\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut child = 0;
    while Instant::now() < deadline && child == 0 {
        child = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|pid| alive(*pid))
            .unwrap_or(0);
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(child != 0, "the child never started");

    drop(pty);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && alive(child) {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!alive(child), "pid {child} outlived the terminal that started it");
}

/// And not even one that has arranged to survive a hangup.
///
/// Closing the pty hangs up the terminal, which is enough for an ordinary
/// child: the kernel sends SIGHUP to the foreground group and it dies. A
/// process that ignores SIGHUP — which is exactly what something meant to
/// outlive its terminal does — sails through that and has to be killed.
#[test]
fn dropping_the_terminal_kills_a_child_that_ignores_a_hangup() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child.pid");
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    // An ignored signal stays ignored across exec, so the sleep inherits it.
    pty.write(b"sh -c 'trap \"\" HUP; echo $$ > child.pid; exec sleep 60'\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut child = 0;
    while Instant::now() < deadline && child == 0 {
        child = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|pid| alive(*pid))
            .unwrap_or(0);
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(child != 0, "the child never started");

    drop(pty);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && alive(child) {
        std::thread::sleep(Duration::from_millis(25));
    }
    let survived = alive(child);
    if survived {
        // Never leave the machine worse than it was found.
        unsafe { libc::kill(child, libc::SIGKILL) };
    }
    assert!(!survived, "pid {child} ignored the hangup and outlived the terminal");
}

/// Whether a process exists, without signalling it.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks; it delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn a_command_that_does_not_exist_fails_rather_than_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/definitely/not/a/shell", dir.path(), 80, 24, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while pty.alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!pty.alive(), "a failed exec should end the session");
}
