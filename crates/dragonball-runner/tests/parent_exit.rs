#![cfg(target_os = "linux")]

use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn parent_exit_terminates_runner() {
    // Adopt the orphan to prove it can be reaped, not merely signal-delivered.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let mut parent = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "parent_helper", "--ignored", "--nocapture"])
        .arg("--test-threads=1")
        .env("AENV_TEST_PARENT_HELPER", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(parent.stdout.take().unwrap());
    let child_pid = loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "parent never reported ready child"
        );
        if let Some(pid) = line.trim().strip_prefix("ready-child:") {
            break pid.parse::<i32>().unwrap();
        }
    };
    parent.kill().unwrap();
    parent.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(child_pid, &mut status, libc::WNOHANG) };
        if result == child_pid {
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
                libc::waitpid(child_pid, &mut status, 0);
            }
            panic!("runner remained alive after parent exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn already_changed_parent_is_rejected() {
    assert!(agentenv_dragonball_runner::process::install_parent_death_guard(u32::MAX).is_err());
}

#[test]
#[ignore = "subprocess helper, invoked only by parent_exit test"]
fn parent_helper() {
    if std::env::var_os("AENV_TEST_PARENT_HELPER").is_none() {
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runner_helper", "--ignored", "--nocapture"])
        .arg("--test-threads=1")
        .env("AENV_TEST_RUNNER_PARENT", std::process::id().to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line.trim() == "guard-ready" {
            break;
        }
    }
    // Serial libtest writes its progress prefix without a trailing newline.
    println!("\nready-child:{}", child.id());
    std::io::stdout().flush().unwrap();
    // The outer test kills this dedicated parent first and then adopts/reaps
    // its child. If that does not happen, waiting is still correct ownership.
    child.wait().unwrap();
}

#[test]
#[ignore = "subprocess helper, invoked only by parent_exit test"]
fn runner_helper() {
    let Some(parent) = std::env::var_os("AENV_TEST_RUNNER_PARENT") else {
        return;
    };
    agentenv_dragonball_runner::process::install_parent_death_guard(
        parent.to_str().unwrap().parse().unwrap(),
    )
    .unwrap();
    println!("\nguard-ready");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}
