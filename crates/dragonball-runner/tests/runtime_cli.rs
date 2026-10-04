#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use agentenv_dragonball_runner::runtime_service::restore_identity;
use agentenv_vmm_protocol::{runtime::*, *};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct RuntimeProcess {
    child: Child,
    root: tempfile::TempDir,
    socket: std::path::PathBuf,
}
impl RuntimeProcess {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.path().join("runtime.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_aenv-dragonball"))
            .arg("--runtime")
            .arg("--socket")
            .arg(&socket)
            .arg("--work-dir")
            .arg(root.path())
            .arg("--instance-id")
            .arg("runtime-test")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut process = Self {
            child,
            root,
            socket,
        };
        let end = Instant::now() + Duration::from_secs(5);
        while !process.socket.exists() && Instant::now() < end {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "runtime runner exited before binding"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(process.socket.exists());
        process
    }
    fn call(&mut self, id: u64, command: RuntimeCommand) -> RuntimeResponse {
        let request = RuntimeRequest {
            mode: "runtime".into(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            request_id: id,
            sandbox_instance_id: "runtime-test".into(),
            budget_ms: 1000,
            command,
        };
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(&encode_runtime_request(&request).unwrap())
            .unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header) as usize;
        assert!(size <= MAX_FRAME_BODY);
        let mut frame = vec![0; 4 + size];
        frame[..4].copy_from_slice(&header);
        stream.read_exact(&mut frame[4..]).unwrap();
        let reply = decode_runtime_response(&frame).unwrap();
        validate_runtime_response(&request, &reply).unwrap();
        reply
    }
    fn stop(&mut self, id: u64) {
        self.call(id, RuntimeCommand::Stop).result.unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < end, "runner must join VMM before exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for RuntimeProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "Linux KVM runtime CLI qualification"]
fn runtime_cli_keeps_probe_separate_and_seccomp_enabled() {
    let mut process = RuntimeProcess::start();
    let RuntimeReply::Hello(hello) = process.call(1, RuntimeCommand::Hello).result.unwrap() else {
        panic!("expected runtime hello");
    };
    assert_eq!(
        hello.dragonball_revision,
        "c77812d669737c11cb16321827acc1c79bf065f3"
    );
    assert_eq!(hello.feature_profile, "db-virtio-blk-net-balloon-v2");
    assert_eq!(
        hello.capabilities,
        [
            "full_memory_capture",
            "held_restore",
            "state_only_capture",
            "leased_memory_export"
        ]
    );
    assert!(!hello.runner_instance_id.is_empty());
    for entry in fs::read_dir(format!("/proc/{}/task", process.child.id())).unwrap() {
        let status = fs::read_to_string(entry.unwrap().path().join("status")).unwrap();
        assert!(status.contains("Seccomp:\t2") && status.contains("NoNewPrivs:\t1"));
    }
    assert_eq!(
        fs::metadata(&process.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut probe = UnixStream::connect(&process.socket).unwrap();
    probe
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    probe
        .write_all(
            &encode_frame(&RequestEnvelope {
                protocol_version: 1,
                request_id: 2,
                sandbox_instance_id: "runtime-test".into(),
                budget_ms: 1000,
                command: ProbeCommand::Hello,
            })
            .unwrap(),
        )
        .unwrap();
    assert_eq!(probe.read(&mut [0; 1]).unwrap(), 0);
    process.stop(2);
    assert!(!process.socket.exists());
}

#[test]
#[ignore = "Linux KVM runtime CLI qualification"]
fn runtime_cli_rejects_probe_file_even_when_request_claims_production() {
    let mut process = RuntimeProcess::start();
    let kernel = process.root.path().join("kernel");
    let disk = process.root.path().join("rootfs");
    let state = process.root.path().join("state");
    let memory = process.root.path().join("memory");
    fs::write(&kernel, b"configuration-only fixture").unwrap();
    fs::File::create(&disk)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    fs::write(&state, br#"{"agentenv_probe":{"probe_only":true}}"#).unwrap();
    fs::File::create(&memory)
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let identity = restore_identity("kernel".into(), "tools".into(), "envd".into());
    process
        .call(
            1,
            RuntimeCommand::Configure(RuntimeVmConfig {
                vcpu_count: 1,
                memory_mib: 128,
                kernel_path: kernel,
                boot_args: "console=ttyS0".into(),
                block_devices: vec![RuntimeBlockDevice {
                    drive_id: "root".into(),
                    path_on_host: disk,
                    read_only: false,
                    is_root: true,
                    is_volume_slot: false,
                    backing_read_only: None,
                    rate_limiter: None,
                }],
                network: RuntimeNetworkDevice {
                    iface_id: "eth0".into(),
                    tap_name: "runtime-tap".into(),
                },
                identity: identity.clone(),
            }),
        )
        .result
        .unwrap();
    let error = process
        .call(
            2,
            RuntimeCommand::LoadHeld(RuntimeSnapshotInput {
                state_path: state,
                memory_path: memory,
                identity,
                probe_only: false,
            }),
        )
        .result
        .unwrap_err();
    assert_eq!(error.code, "snapshot_preflight_error");
    let RuntimeReply::Status(status) = process.call(3, RuntimeCommand::Status).result.unwrap()
    else {
        panic!("expected status");
    };
    assert_eq!(status.phase, RuntimePhase::Configured);
    process.stop(4);
}

#[test]
#[ignore = "Linux KVM runtime CLI qualification"]
fn runtime_cli_frame_read_deadline_is_total_not_per_byte() {
    let mut process = RuntimeProcess::start();
    let mut slow = UnixStream::connect(&process.socket).unwrap();
    let request = RuntimeRequest {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: 0,
        sandbox_instance_id: "runtime-test".into(),
        budget_ms: 1000,
        command: RuntimeCommand::Hello,
    };
    let frame = encode_runtime_request(&request).unwrap();
    slow.write_all(&frame[..5]).unwrap();
    let writer = std::thread::spawn(move || {
        for byte in &frame[5..11] {
            std::thread::sleep(Duration::from_millis(400));
            if slow.write_all(&[*byte]).is_err() {
                return;
            }
        }
        let _ = slow.write_all(&frame[11..]);
    });
    let started = Instant::now();
    let reply = process.call(1, RuntimeCommand::Hello).result;
    let elapsed = started.elapsed();
    writer.join().unwrap();
    process.stop(2);
    assert!(reply.is_ok());
    assert!(
        elapsed < Duration::from_millis(1600),
        "drip peer blocked serial runtime for {elapsed:?}"
    );
}
