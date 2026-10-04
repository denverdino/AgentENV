#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::{
    os::unix::process::ExitStatusExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn interrupted_poll_can_restart_after_stop_and_continue() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "restart_helper", "--ignored", "--nocapture"])
        .env("AENV_RESTART_FILTER_TEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut reaped = false;
    let result = (|| -> Result<(), String> {
        // Wait for the real blocked syscall, not a timing-based sleep before STOP.
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("helper exited before poll: {status}"));
            }
            let blocked = std::fs::read_dir(format!("/proc/{}/task", child.id()))
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
                .any(|task| {
                    std::fs::read_to_string(task.path().join("syscall"))
                        .ok()
                        .and_then(|line| line.split_whitespace().next()?.parse::<i64>().ok())
                        == Some(libc::SYS_poll)
                });
            if blocked {
                break;
            }
            if Instant::now() >= until {
                return Err("helper did not enter poll".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let pid = child.id() as libc::pid_t;
        if unsafe { libc::kill(pid, libc::SIGSTOP) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let mut status = 0;
            let waited =
                unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if waited == pid {
                if libc::WIFSTOPPED(status) {
                    break;
                }
                reaped = libc::WIFEXITED(status) || libc::WIFSIGNALED(status);
                return Err(format!("helper exited before stop: wait status {status}"));
            }
            if waited < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
            }
            if Instant::now() >= until {
                return Err("helper did not stop".into());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if unsafe { libc::kill(pid, libc::SIGCONT) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("restarted poll helper failed: {status}"))
                };
            }
            if Instant::now() >= until {
                return Err("restarted poll did not finish".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if !reaped && child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        child.wait().unwrap();
    }
    result.unwrap();
}

#[test]
#[ignore = "subprocess helper only"]
fn restart_helper() {
    if std::env::var_os("AENV_RESTART_FILTER_TEST").is_none() {
        return;
    }
    agentenv_dragonball_runner::seccomp::install_m0_filter().unwrap();
    let mut descriptor = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // Parent keeps the stdin pipe open and empty. A normal restart times out.
    assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 4000) }, 0);
}

#[test]
fn filter_is_inherited_and_forbidden_calls_kill_whole_process() {
    for forbidden in [false, true] {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "filter_helper", "--ignored", "--nocapture"])
            .env(
                "AENV_FILTER_TEST",
                if forbidden { "forbidden" } else { "inherit" },
            )
            .status()
            .unwrap();
        if forbidden {
            assert_eq!(status.signal(), Some(libc::SIGSYS));
        } else {
            assert!(status.success());
        }
    }
}

#[test]
#[ignore = "subprocess helper only"]
fn filter_helper() {
    let Some(mode) = std::env::var_os("AENV_FILTER_TEST") else {
        return;
    };
    agentenv_dragonball_runner::seccomp::install_m0_filter().unwrap();
    if mode == "forbidden" {
        unsafe {
            libc::syscall(libc::SYS_mount, 0, 0, 0, 0, 0);
        }
        std::process::exit(0);
    }
    let mode = std::thread::spawn(|| unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) })
        .join()
        .unwrap();
    assert_eq!(mode, 2);
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
        1
    );
}
