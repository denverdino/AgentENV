#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use agentenv_vmm_protocol::*;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Runner {
    child: Child,
    socket: PathBuf,
    _work: tempfile::TempDir,
}

impl Runner {
    fn start() -> Self {
        let work = tempfile::tempdir().unwrap();
        fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = work.path().join("vmm.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_aenv-dragonball"))
            .arg("--socket")
            .arg(&socket)
            .arg("--work-dir")
            .arg(work.path())
            .args([
                "--instance-id",
                "cli-probe",
                "--parent-pid",
                &std::process::id().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child,
            socket,
            _work: work,
        }
    }

    fn connect(&mut self) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut stderr = String::new();
                self.child
                    .stderr
                    .as_mut()
                    .unwrap()
                    .read_to_string(&mut stderr)
                    .unwrap();
                panic!("runner exited before ready: {status}, {stderr}");
            }
            match UnixStream::connect(&self.socket) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return stream;
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("runner socket not ready: {error}"),
            }
        }
    }

    fn call(&mut self, id: u64, instance: &str, command: ProbeCommand) -> ResponseEnvelope {
        let req = RequestEnvelope {
            protocol_version: 1,
            request_id: id,
            sandbox_instance_id: instance.into(),
            budget_ms: 1000,
            command,
        };
        let mut stream = self.connect();
        stream.write_all(&encode_frame(&req).unwrap()).unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let length = u32::from_be_bytes(header) as usize;
        assert!(length <= MAX_FRAME_BODY);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let response = decode_response(&[header.as_slice(), &body].concat()).unwrap();
        if instance == "cli-probe" {
            validate_response(&req, &response).unwrap();
        }
        response
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "runner did not exit after stop");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "Linux KVM qualification; no guest boot or network capability required"]
fn runner_cli_reports_ready_and_stops_without_boot() {
    let mut runner = Runner::start();
    let reply = runner
        .call(1, "cli-probe", ProbeCommand::Hello)
        .result
        .unwrap();
    let ProbeReply::Hello(identity) = reply else {
        panic!("expected ready identity");
    };
    assert_eq!(
        identity.dragonball_revision,
        "c77812d669737c11cb16321827acc1c79bf065f3"
    );
    assert_eq!(
        fs::metadata(&runner.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for entry in fs::read_dir(format!("/proc/{}/task", runner.child.id())).unwrap() {
        let status = fs::read_to_string(entry.unwrap().path().join("status")).unwrap();
        assert!(status.contains("Seccomp:\t2"));
        assert!(status.contains("NoNewPrivs:\t1"));
    }
    assert_eq!(
        runner
            .call(2, "other-instance", ProbeCommand::Start)
            .result
            .unwrap_err()
            .code,
        "wrong_instance"
    );
    assert_eq!(
        runner
            .call(2, "cli-probe", ProbeCommand::Status)
            .result
            .unwrap(),
        ProbeReply::Status(ProbePhase::Unconfigured)
    );
    let mut stream = runner.connect();
    stream.write_all(&1_048_577_u32.to_be_bytes()).unwrap();
    assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
    assert!(runner
        .call(3, "cli-probe", ProbeCommand::Stop)
        .result
        .is_ok());
    assert!(runner.wait_exit().success());
    assert!(!runner.socket.exists());
}

#[test]
#[ignore = "Linux KVM qualification; inject failure before guest boot"]
fn missing_kernel_cleans_up_master_worker_and_socket() {
    let mut runner = Runner::start();
    let config = ProbeVmConfig {
        vcpu_count: 1,
        memory_mib: 128,
        kernel_path: runner._work.path().join("absent-kernel"),
        boot_args: String::new(),
        block_devices: vec![ProbeBlockDevice {
            drive_id: "tools".into(),
            path_on_host: "/absent-tools".into(),
            read_only: true,
            is_root: true,
        }],
        network: ProbeNetworkDevice {
            iface_id: "eth0".into(),
            tap_name: "tap0".into(),
        },
    };
    let error = runner
        .call(1, "cli-probe", ProbeCommand::Configure(config))
        .result
        .unwrap_err();
    assert_eq!(error.code, "vmm_error");
    assert!(error.message.contains("kernel file does not exist"));
    assert!(!runner.wait_exit().success());
    assert!(!runner.socket.exists());
}

#[test]
#[ignore = "Linux KVM qualification; checkpoint IO failure before guest boot"]
fn checkpoint_preflight_io_error_is_queryable_over_socket() {
    let mut runner = Runner::start();
    let kernel = runner._work.path().join("kernel");
    let tools = runner._work.path().join("tools");
    fs::write(&kernel, [0; 4096]).unwrap();
    fs::write(&tools, [0; 4096]).unwrap();
    let config = ProbeVmConfig {
        vcpu_count: 1,
        memory_mib: 128,
        kernel_path: kernel,
        boot_args: String::new(),
        block_devices: vec![ProbeBlockDevice {
            drive_id: "tools".into(),
            path_on_host: tools,
            read_only: true,
            is_root: true,
        }],
        network: ProbeNetworkDevice {
            iface_id: "eth0".into(),
            tap_name: "tap0".into(),
        },
    };
    assert!(runner
        .call(1, "cli-probe", ProbeCommand::Configure(config))
        .result
        .is_ok());
    let state_path = runner._work.path().join("absent-state");
    let memory_path = runner._work.path().join("absent-memory");
    let error = runner
        .call(
            2,
            "cli-probe",
            ProbeCommand::RestoreQuiesced {
                state_path,
                memory_path,
            },
        )
        .result
        .unwrap_err();
    assert_eq!(error.code, "snapshot_preflight_error");
    assert_eq!(
        runner
            .call(3, "cli-probe", ProbeCommand::Status)
            .result
            .unwrap(),
        ProbeReply::Status(ProbePhase::Configured)
    );
    assert!(runner
        .call(4, "cli-probe", ProbeCommand::Stop)
        .result
        .is_ok());
    assert!(runner.wait_exit().success());
    assert!(!runner.socket.exists());
}
