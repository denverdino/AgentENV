use agentenv_dragonball_runner::runtime_service::{RuntimeService, RuntimeVmm};
use agentenv_vmm_protocol::runtime::*;
use agentenv_vmm_protocol::runtime_memory::*;
use std::path::Path;

#[derive(Default)]
struct TestVmm {
    actions: Vec<&'static str>,
    fail_export: bool,
    omit_net: bool,
    fail_map: bool,
    fail_stop: bool,
}

impl RuntimeVmm for TestVmm {
    fn configure(&mut self, _: &RuntimeVmConfig) -> anyhow::Result<()> {
        self.actions.push("configure");
        Ok(())
    }
    fn start(&mut self) -> anyhow::Result<()> {
        self.actions.push("start");
        Ok(())
    }
    fn begin_capture(&mut self, generation: u64) -> anyhow::Result<CaptureReport> {
        self.actions.push("begin");
        let mut report = CaptureReport {
            token: CaptureToken {
                runner_instance_id: String::new(),
                generation,
            },
            memory_layout: MemoryLayout {
                page_size: 4096,
                image_size: 128 * 1024 * 1024,
                regions: vec![MemoryRegion {
                    guest_base: 0,
                    image_offset: 0,
                    length: 128 * 1024 * 1024,
                }],
            },
            vcpu_ids: vec![0],
            worker_acks: vec![
                WorkerAck {
                    device_id: "balloon:memory".into(),
                    generation,
                    pending_io: 0,
                    memory_writers: 0,
                    flush_completed: false,
                },
                WorkerAck {
                    device_id: "block:root/q0".into(),
                    generation,
                    pending_io: 0,
                    memory_writers: 0,
                    flush_completed: true,
                },
            ],
        };
        if !self.omit_net {
            report.worker_acks.push(WorkerAck {
                device_id: "net:eth0".into(),
                generation,
                pending_io: 0,
                memory_writers: 0,
                flush_completed: false,
            });
        }
        Ok(report)
    }
    fn export_held(&mut self, _: u64, _: &Path, _: &Path) -> anyhow::Result<()> {
        self.actions.push("export");
        anyhow::ensure!(
            !self.fail_export,
            "response deadline exceeded; outcome unknown"
        );
        Ok(())
    }
    fn end_capture(&mut self, _: u64) -> anyhow::Result<()> {
        self.actions.push("end");
        Ok(())
    }
    fn export_memory_map(&mut self, generation: u64) -> anyhow::Result<FrozenMemoryMap> {
        self.actions.push("map");
        anyhow::ensure!(!self.fail_map, "map response lost; pin outcome unknown");
        Ok(FrozenMemoryMap {
            token: CaptureToken {
                runner_instance_id: String::new(),
                generation,
            },
            layout_id: config().identity.memory_layout_id,
            page_size: 4096,
            regions: vec![FrozenMemoryRegion {
                guest_base: 0,
                host_base: 0x1_0000_0000,
                image_offset: 0,
                length: 128 * 1024 * 1024,
            }],
            changed_ranges: (0..2049)
                .map(|n| MemoryRange {
                    image_offset: n * 8192,
                    length: 4096,
                })
                .collect(),
            zero_ranges: vec![],
        })
    }
    fn release_memory_map(&mut self, _: u64) -> anyhow::Result<()> {
        self.actions.push("release_map");
        Ok(())
    }
    fn load_held(&mut self, _: u64, _: &RuntimeSnapshotInput) -> anyhow::Result<CaptureReport> {
        self.actions.push("load");
        anyhow::bail!("not used by this fixture")
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.actions.push("stop");
        anyhow::ensure!(!self.fail_stop, "teardown not confirmed");
        Ok(())
    }
}

#[test]
fn m2_unknown_map_outcome_retains_pin_until_confirmed_teardown() {
    let (mut service, token) = held();
    service.vmm.fail_map = true;
    service.vmm.fail_stop = true;
    assert_eq!(
        service
            .handle(request(
                4,
                RuntimeCommand::ExportMemoryMap { token, cursor: 0 }
            ))
            .result
            .unwrap_err()
            .code,
        "vmm_error"
    );
    let RuntimeReply::Status(status) = service
        .handle(request(5, RuntimeCommand::Status))
        .result
        .unwrap()
    else {
        panic!("missing status")
    };
    assert_eq!(status.phase, RuntimePhase::Failed);
    assert!(
        status.memory_map_pinned,
        "unknown export plus failed teardown must not claim pin released"
    );
    service.vmm.fail_stop = false;
    service
        .handle(request(6, RuntimeCommand::Stop))
        .result
        .unwrap();
    let RuntimeReply::Status(status) = service
        .handle(request(7, RuntimeCommand::Status))
        .result
        .unwrap()
    else {
        panic!("missing status")
    };
    assert!(!status.memory_map_pinned);
}

fn request(id: u64, command: RuntimeCommand) -> RuntimeRequest {
    RuntimeRequest {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: id,
        sandbox_instance_id: "sandbox".into(),
        budget_ms: 1000,
        command,
    }
}
fn config() -> RuntimeVmConfig {
    RuntimeVmConfig {
        vcpu_count: 1,
        memory_mib: 128,
        kernel_path: "/inputs/kernel".into(),
        boot_args: "console=ttyS0".into(),
        block_devices: vec![RuntimeBlockDevice {
            drive_id: "root".into(),
            path_on_host: "/inputs/disk".into(),
            read_only: false,
            is_root: true,
            is_volume_slot: false,
            backing_read_only: None,
            rate_limiter: None,
        }],
        network: RuntimeNetworkDevice {
            iface_id: "eth0".into(),
            tap_name: "tap0".into(),
        },
        identity: agentenv_dragonball_runner::runtime_service::restore_identity(
            "kernel".into(),
            "tools".into(),
            "envd".into(),
        ),
    }
}
fn configured() -> RuntimeService<TestVmm> {
    let mut service = RuntimeService::new("sandbox".into(), "runner-1".into(), TestVmm::default());
    assert!(service
        .handle(request(1, RuntimeCommand::Configure(config())))
        .result
        .is_ok());
    service
}

#[test]
fn runtime_rejects_worker_set_larger_than_protocol_before_configuring_vmm() {
    let mut machine = config();
    for index in 1..256 {
        machine.block_devices.push(RuntimeBlockDevice {
            drive_id: format!("disk-{index}"),
            path_on_host: "/inputs/disk".into(),
            read_only: false,
            is_root: false,
            is_volume_slot: false,
            backing_read_only: None,
            rate_limiter: None,
        });
    }
    let mut service = RuntimeService::new("sandbox".into(), "runner-1".into(), TestVmm::default());
    let response = service.handle(request(1, RuntimeCommand::Configure(machine)));
    assert!(
        response.result.is_err(),
        "every configured machine must have representable held proof"
    );
    assert_eq!(service.phase(), RuntimePhase::Unconfigured);
    assert!(service.vmm.actions.is_empty());
}
fn held() -> (RuntimeService<TestVmm>, CaptureToken) {
    let mut service = configured();
    assert!(service
        .handle(request(2, RuntimeCommand::Start))
        .result
        .is_ok());
    let reply = service
        .handle(request(3, RuntimeCommand::BeginCapture))
        .result
        .unwrap();
    let RuntimeReply::Held(report) = reply else {
        panic!("expected held proof");
    };
    (service, report.token)
}

#[test]
fn m2_map_pin_blocks_resume_until_explicit_release() {
    let (mut service, token) = held();
    let RuntimeReply::MemoryMap(first) = service
        .handle(request(
            4,
            RuntimeCommand::ExportMemoryMap {
                token: token.clone(),
                cursor: 0,
            },
        ))
        .result
        .unwrap()
    else {
        panic!("expected first map page")
    };
    assert_eq!(first.next_cursor, Some(1024));
    assert_eq!(first.token, token);
    let RuntimeReply::Status(status) = service
        .handle(request(5, RuntimeCommand::Status))
        .result
        .unwrap()
    else {
        panic!("status")
    };
    assert!(status.memory_map_pinned);
    assert!(service
        .handle(request(
            6,
            RuntimeCommand::EndCapture {
                token: token.clone()
            }
        ))
        .result
        .is_err());
    assert_eq!(service.phase(), RuntimePhase::Held);
    assert!(service
        .handle(request(
            7,
            RuntimeCommand::ExportMemoryMap {
                token: token.clone(),
                cursor: 0
            }
        ))
        .result
        .is_err());
    assert!(service
        .handle(request(
            8,
            RuntimeCommand::ExportMemoryMap {
                token: token.clone(),
                cursor: 2048
            }
        ))
        .result
        .is_err());
    assert!(service
        .handle(request(
            9,
            RuntimeCommand::ExportMemoryMap {
                token: token.clone(),
                cursor: 1024
            }
        ))
        .result
        .is_ok());
    let RuntimeReply::MemoryMap(last) = service
        .handle(request(
            10,
            RuntimeCommand::ExportMemoryMap {
                token: token.clone(),
                cursor: 2048,
            },
        ))
        .result
        .unwrap()
    else {
        panic!("last page")
    };
    assert!(last.next_cursor.is_none());
    assert!(service
        .handle(request(
            11,
            RuntimeCommand::EndCapture {
                token: token.clone()
            }
        ))
        .result
        .is_err());
    let mut wrong = token.clone();
    wrong.generation += 1;
    assert!(service
        .handle(request(
            12,
            RuntimeCommand::ReleaseMemoryMap { token: wrong }
        ))
        .result
        .is_err());
    assert!(service
        .handle(request(
            13,
            RuntimeCommand::ReleaseMemoryMap {
                token: token.clone()
            }
        ))
        .result
        .is_ok());
    assert!(service
        .handle(request(14, RuntimeCommand::EndCapture { token }))
        .result
        .is_ok());
    assert_eq!(service.phase(), RuntimePhase::Running);
    assert_eq!(
        service.vmm.actions,
        ["configure", "start", "begin", "map", "release_map", "end"]
    );
}

#[test]
fn m2_reader_rejects_m1_identity_without_changing_record() {
    let mut old = config().identity;
    old.dragonball_revision = "0bc22d5ce53a072aabf4fe42c133dacefeaa9c72".into();
    old.snapshot_format_epoch = 1;
    old.restore_abi_id =
        "dragonball-held-fullram-v1@0bc22d5ce53a072aabf4fe42c133dacefeaa9c72".into();
    old.device_profile_id = "mmio-rawblk-net-v1".into();
    old.memory_layout_id = "packed-ram-4k-v1".into();
    let record = serde_json::to_vec(&old).unwrap();
    assert!(old.validate_profile().is_err());
    assert_eq!(record, serde_json::to_vec(&old).unwrap());
}

#[test]
fn runtime_rejects_probe_snapshot() {
    let mut service = configured();
    let result = service.handle(request(
        2,
        RuntimeCommand::LoadHeld(RuntimeSnapshotInput {
            state_path: "/inputs/state".into(),
            memory_path: "/inputs/memory".into(),
            identity: config().identity,
            probe_only: true,
        }),
    ));
    assert!(result.result.is_err());
    assert_eq!(service.phase(), RuntimePhase::Configured);
    assert_eq!(service.vmm.actions, ["configure"]);
}

#[test]
fn runtime_config_requires_the_current_process_build_provenance() {
    let mut machine = config();
    agentenv_dragonball_runner::runtime_service::validate_runtime_config(&machine).unwrap();
    machine.identity.runner_build_id = format!("sha256:{}", "b".repeat(64));
    machine.identity.dragonball_revision = "c77812d669737c11cb16321827acc1c79bf065f3".into();
    assert!(
        agentenv_dragonball_runner::runtime_service::validate_runtime_config(&machine).is_err()
    );
    machine = config();
    machine.identity.restore_abi_id = "dragonball-held-cow-v3".into();
    assert!(
        agentenv_dragonball_runner::runtime_service::validate_runtime_config(&machine).is_err()
    );
}

#[test]
fn runtime_load_checks_abi_before_calling_the_vmm() {
    let mut service = configured();
    let mut identity = config().identity;
    identity.restore_abi_id = "dragonball-held-cow-v3".into();
    let input = RuntimeSnapshotInput {
        state_path: "/inputs/state".into(),
        memory_path: "/inputs/memory".into(),
        identity,
        probe_only: false,
    };
    assert!(service
        .handle(request(2, RuntimeCommand::LoadHeld(input)))
        .result
        .is_err());
    assert_eq!(service.phase(), RuntimePhase::Configured);
    assert_eq!(service.vmm.actions, ["configure"]);
}

#[test]
fn runtime_load_accepts_another_build_with_the_same_abi() {
    let mut service = configured();
    let mut identity = config().identity;
    identity.runner_build_id = format!("sha256:{}", "b".repeat(64));
    identity.dragonball_revision = "c77812d669737c11cb16321827acc1c79bf065f3".into();
    let input = RuntimeSnapshotInput {
        state_path: "/inputs/state".into(),
        memory_path: "/inputs/memory".into(),
        identity,
        probe_only: false,
    };
    // The fixture fails load deliberately; reaching it proves identity preflight passed.
    assert!(service
        .handle(request(2, RuntimeCommand::LoadHeld(input)))
        .result
        .is_err());
    assert_eq!(service.vmm.actions, ["configure", "load", "stop"]);
}

#[test]
fn runtime_rejects_stale_capture_token() {
    let (mut service, mut token) = held();
    token.generation += 1;
    assert!(service
        .handle(request(
            4,
            RuntimeCommand::ExportHeldSnapshot {
                token,
                state_path: "/work/state".into(),
                memory_path: "/work/memory".into()
            }
        ))
        .result
        .is_err());
    assert_eq!(service.phase(), RuntimePhase::Held);
    assert_eq!(service.vmm.actions, ["configure", "start", "begin"]);
}

#[test]
fn runtime_capture_response_has_all_worker_acks() {
    let (mut service, token) = held();
    assert_eq!(token.runner_instance_id, "runner-1");
    let RuntimeReply::Status(status) = service
        .handle(request(4, RuntimeCommand::Status))
        .result
        .unwrap()
    else {
        panic!("expected status");
    };
    let report = status.capture.unwrap();
    assert_eq!(
        report
            .worker_acks
            .iter()
            .map(|ack| ack.device_id.as_str())
            .collect::<Vec<_>>(),
        ["balloon:memory", "block:root/q0", "net:eth0"]
    );
    let mut incomplete = configured();
    incomplete.vmm.omit_net = true;
    incomplete
        .handle(request(2, RuntimeCommand::Start))
        .result
        .unwrap();
    assert!(incomplete
        .handle(request(3, RuntimeCommand::BeginCapture))
        .result
        .is_err());
    assert_eq!(incomplete.phase(), RuntimePhase::Failed);
    assert_eq!(incomplete.vmm.actions.last(), Some(&"stop"));
}

#[test]
fn runtime_timeout_does_not_reissue_export() {
    let (mut service, token) = held();
    service.vmm.fail_export = true;
    let command = RuntimeCommand::ExportHeldSnapshot {
        token,
        state_path: "/work/state".into(),
        memory_path: "/work/memory".into(),
    };
    assert!(service.handle(request(4, command.clone())).result.is_err());
    assert_eq!(service.phase(), RuntimePhase::Failed);
    assert!(service.handle(request(5, command)).result.is_err());
    assert_eq!(
        service
            .vmm
            .actions
            .iter()
            .filter(|action| **action == "export")
            .count(),
        1
    );
    assert_eq!(service.vmm.actions.last(), Some(&"stop"));
    let RuntimeReply::Status(status) = service
        .handle(request(6, RuntimeCommand::Status))
        .result
        .unwrap()
    else {
        panic!("expected status");
    };
    assert!(
        status.capture.is_none(),
        "unknown outcome must not expose a confirmed held proof"
    );
}

#[test]
fn runtime_rejects_previous_runner_and_replay_without_mutation() {
    let (mut service, mut token) = held();
    token.runner_instance_id = "previous-process".into();
    assert!(service
        .handle(request(4, RuntimeCommand::EndCapture { token }))
        .result
        .is_err());
    assert!(service
        .handle(request(3, RuntimeCommand::BeginCapture))
        .result
        .is_err());
    let mut neighbor = request(5, RuntimeCommand::Stop);
    neighbor.sandbox_instance_id = "neighbor".into();
    assert!(service.handle(neighbor).result.is_err());
    assert_eq!(service.phase(), RuntimePhase::Held);
    assert_eq!(service.vmm.actions, ["configure", "start", "begin"]);
}

#[test]
fn runtime_release_invalidates_old_token() {
    let (mut service, token) = held();
    service
        .handle(request(
            4,
            RuntimeCommand::EndCapture {
                token: token.clone(),
            },
        ))
        .result
        .unwrap();
    assert_eq!(service.phase(), RuntimePhase::Running);
    assert!(service
        .handle(request(
            5,
            RuntimeCommand::ExportHeldSnapshot {
                token: token.clone(),
                state_path: "/work/state".into(),
                memory_path: "/work/memory".into()
            }
        ))
        .result
        .is_err());
    let RuntimeReply::Held(next) = service
        .handle(request(6, RuntimeCommand::BeginCapture))
        .result
        .unwrap()
    else {
        panic!("expected held report");
    };
    assert!(next.token.generation > token.generation);
    assert!(service
        .handle(request(7, RuntimeCommand::EndCapture { token }))
        .result
        .is_err());
}

#[test]
fn runtime_hello_has_content_build_identity_and_fixed_abi() {
    let mut service = RuntimeService::new("sandbox".into(), "runner-1".into(), TestVmm::default());
    let RuntimeReply::Hello(hello) = service
        .handle(request(1, RuntimeCommand::Hello))
        .result
        .unwrap()
    else {
        panic!("expected hello");
    };
    let hash = hello
        .runner_build_id
        .strip_prefix("sha256:")
        .expect("build ID must fingerprint source, not just package/upstream versions");
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(hello.restore_abi_id, hello.dragonball_revision);
    assert_eq!(hello.restore_abi_id, RESTORE_ABI_ID);
}
