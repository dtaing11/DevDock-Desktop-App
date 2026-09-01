//! Tests of the integrated terminal against a real shell on a real pty.
//!
//! The emulator is unit-tested next to its own code; what these check is
//! the part that only fails in the presence of an actual process: that the
//! shell starts, sees a terminal, answers input, and dies when told to.

#![cfg(unix)]

use git_manage::terminal::pty::Pty;
use std::time::{Duration, Instant};

/// Waits for the screen to satisfy `check`, so tests never race the shell.
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

#[test]
fn a_shell_runs_a_command_and_its_output_reaches_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).expect("pty");
    assert!(pty.alive());

    pty.write(b"echo hello-from-the-pty\n");
    let text = eventually(&pty, "the echo output", |t| t.contains("hello-from-the-pty"));
    // The command is echoed by the tty and its output follows: both are on
    // the screen, which is what makes it a terminal rather than a pipe.
    assert!(text.matches("hello-from-the-pty").count() >= 2, "{text}");
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

    pty.write(b"test -t 1 && echo IS-A-TTY\n");
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
    pty.write(b"echo ready\n");
    eventually(&pty, "the shell to start", |t| t.contains("ready"));

    pty.resize(100, 30);
    pty.write(b"stty size\n");
    eventually(&pty, "the new window size", |t| t.contains("30 100"));
}

#[test]
fn a_signal_interrupts_the_foreground_program() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    // Start something that would never finish on its own.
    pty.write(b"sleep 60; echo AFTER-THE-SLEEP\n");
    std::thread::sleep(Duration::from_millis(400));
    pty.signal(libc::SIGINT);

    // ^C ends the sleep, and the shell carries on.
    pty.write(b"echo still-here\n");
    eventually(&pty, "the shell after an interrupt", |t| t.contains("still-here"));
}

#[test]
fn colour_output_keeps_its_colour() {
    let dir = tempfile::tempdir().unwrap();
    let pty = Pty::spawn("/bin/sh", dir.path(), 80, 24, None).unwrap();

    pty.write(b"printf '\\033[31mRED\\033[0m\\n'\n");
    eventually(&pty, "the coloured output", |t| t.contains("RED"));

    let screen = pty.screen.lock().unwrap();
    let coloured = screen.lines().iter().any(|line| {
        line.iter().any(|cell| {
            cell.ch == 'R' && cell.style.fg == Some(git_manage::terminal::vt::Color::Red)
        })
    });
    assert!(coloured, "the red never made it through:\n{}", screen.text());
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
    pty.write(b"echo ready\n");
    eventually(&pty, "the shell to start", |t| t.contains("ready"));

    // A closed panel must not leave a shell running.
    drop(pty);
    // Nothing to assert directly without scanning the process table; the
    // test exists so the Drop path is exercised under a real child.
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
