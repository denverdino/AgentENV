use agentenv_dragonball_runner::{
    probe_snapshot::{prepare_outputs, validate_snapshot},
    service::{ProbeVmm, RunnerService},
};
use agentenv_vmm_protocol::*;
use serde_json::json;
use std::path::Path;

#[derive(Default)]
struct Fake {
    saves: usize,
    restores: usize,
    output_root: Option<std::path::PathBuf>,
    stops: usize,
}

#[cfg(unix)]
#[test]
fn m2_state_only_output_is_new_owned_and_confined() {
    use agentenv_dragonball_runner::probe_snapshot::prepare_state_output;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let state = root.path().join("state.json");
    assert!(prepare_state_output(root.path(), &outside.path().join("state")).is_err());
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    assert!(prepare_state_output(root.path(), &root.path().join("escape/state")).is_err());
    let file = prepare_state_output(root.path(), &state).unwrap();
    file.set_len(17).unwrap();
    assert!(prepare_state_output(root.path(), &state).is_err());
    assert_eq!(std::fs::metadata(&state).unwrap().len(), 17);
    assert!(!root.path().join("memory.bin").exists());
}

#[cfg(unix)]
#[test]
fn save_outputs_reject_escape_symlink_and_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let state = outside.path().join("escaped.json");
    let memory = outside.path().join("escaped.mem");
    assert!(
        prepare_outputs(root.path(), &state, &memory).is_err(),
        "output escaped the instance root"
    );
    let link = root.path().join("link");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    assert!(prepare_outputs(
        root.path(),
        &link.join("state.json"),
        &link.join("memory.bin")
    )
    .is_err());
    let state = root.path().join("state.json");
    let memory = root.path().join("memory.bin");
    std::fs::write(&state, b"keep").unwrap();
    assert!(prepare_outputs(root.path(), &state, &memory).is_err());
    assert_eq!(std::fs::read(&state).unwrap(), b"keep");
    std::fs::remove_file(&state).unwrap();
    assert!(prepare_outputs(root.path(), &state, &memory).is_ok());
}
impl ProbeVmm for Fake {
    fn configure(&mut self, _: &ProbeVmConfig) -> anyhow::Result<()> {
        Ok(())
    }
    fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.stops += 1;
        Ok(())
    }
    fn save_quiesced(&mut self, state: &Path, memory: &Path) -> anyhow::Result<()> {
        if let Some(root) = &self.output_root {
            agentenv_dragonball_runner::probe_snapshot::prepare_outputs(root, state, memory)?;
        }
        self.saves += 1;
        Ok(())
    }
    fn restore_quiesced(&mut self, _: &Path, _: &Path) -> anyhow::Result<()> {
        self.restores += 1;
        Ok(())
    }
}
fn config() -> ProbeVmConfig {
    ProbeVmConfig {
        vcpu_count: 1,
        memory_mib: 1,
        kernel_path: "/kernel".into(),
        boot_args: String::new(),
        block_devices: vec![ProbeBlockDevice {
            drive_id: "tools".into(),
            path_on_host: "/tools".into(),
            read_only: true,
            is_root: true,
        }],
        network: ProbeNetworkDevice {
            iface_id: "eth0".into(),
            tap_name: "tap0".into(),
        },
    }
}
fn request(id: u64, command: ProbeCommand) -> RequestEnvelope {
    RequestEnvelope {
        protocol_version: 1,
        request_id: id,
        sandbox_instance_id: "source".into(),
        budget_ms: 1000,
        command,
    }
}
fn save() -> ProbeCommand {
    ProbeCommand::SaveQuiesced {
        state_path: "/work/state.json".into(),
        memory_path: "/work/memory.bin".into(),
    }
}
#[test]
fn save_quiesced_requires_paused_probe() {
    let mut s = RunnerService::new("source".into(), Fake::default());
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    assert!(s.handle(request(2, ProbeCommand::Start)).result.is_ok());
    assert_eq!(
        s.handle(request(3, save())).result.unwrap_err().code,
        "invalid_phase"
    );
    assert_eq!(s.vmm.saves, 0);
    assert!(s.handle(request(4, ProbeCommand::Pause)).result.is_ok());
    assert!(s.handle(request(5, save())).result.is_ok());
    assert_eq!(s.vmm.saves, 1);
    assert_eq!(s.phase(), ProbePhase::Paused);
}

#[test]
fn snapshot_preflight_io_error_keeps_state_queryable_and_stop_cleans_up() {
    let work = tempfile::tempdir().unwrap();
    let mut s = RunnerService::new(
        "source".into(),
        Fake {
            output_root: Some(work.path().to_owned()),
            ..Fake::default()
        },
    );
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    assert!(s.handle(request(2, ProbeCommand::Start)).result.is_ok());
    assert!(s.handle(request(3, ProbeCommand::Pause)).result.is_ok());
    let error = s
        .handle(request(
            4,
            ProbeCommand::SaveQuiesced {
                state_path: work.path().join("missing/state"),
                memory_path: work.path().join("missing/memory"),
            },
        ))
        .result
        .unwrap_err();
    assert_eq!(error.code, "snapshot_preflight_error");
    assert_eq!(
        s.handle(request(5, ProbeCommand::Status)).result.unwrap(),
        ProbeReply::Status(ProbePhase::Paused)
    );
    assert_eq!(s.vmm.saves, 0);
    assert_eq!(s.vmm.stops, 0);
    assert!(s.handle(request(6, ProbeCommand::Stop)).result.is_ok());
    assert_eq!(s.vmm.stops, 1);
}
#[test]
fn restore_only_runs_on_fresh_configured_probe() {
    let mut s = RunnerService::new("source".into(), Fake::default());
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    let restore = || ProbeCommand::RestoreQuiesced {
        state_path: "/source/state.json".into(),
        memory_path: "/source/memory.bin".into(),
    };
    assert!(s.handle(request(2, restore())).result.is_ok());
    assert_eq!(s.phase(), ProbePhase::Running);
    assert_eq!(
        s.handle(request(3, restore())).result.unwrap_err().code,
        "invalid_phase"
    );
    assert_eq!(s.vmm.restores, 1);
}
#[test]
fn quiesced_restore_rejects_corrupt_or_mismatched_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let memory = dir.path().join("memory.bin");
    std::fs::File::create(&memory)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    let machine = config();
    let valid = valid_state(&machine);
    std::fs::write(&state, valid.to_string()).unwrap();
    assert!(validate_snapshot(&state, &memory, &machine).is_ok());
    std::fs::write(&state, b"{").unwrap();
    assert!(
        validate_snapshot(&state, &memory, &machine).is_err(),
        "truncated JSON accepted"
    );
    for modified in [
        {
            let mut v = valid.clone();
            v["memory_state"]["regions"][0]["guest_addr"] = json!(4096);
            v
        },
        {
            let mut v = valid.clone();
            v["vcpu_states"] = json!([]);
            v
        },
        {
            let mut v = valid.clone();
            v["agentenv_probe"]["identity"]["feature_profile"] = json!("other");
            v
        },
        {
            let mut v = valid.clone();
            v["agentenv_probe"]["machine"]["vcpu_count"] = json!(2);
            v
        },
    ] {
        std::fs::write(&state, modified.to_string()).unwrap();
        assert!(
            validate_snapshot(&state, &memory, &machine).is_err(),
            "mismatched state accepted"
        );
    }
    std::fs::write(&state, valid.to_string()).unwrap();
    std::fs::File::create(&memory)
        .unwrap()
        .set_len(4096)
        .unwrap();
    assert!(
        validate_snapshot(&state, &memory, &machine).is_err(),
        "short memory accepted"
    );
}

// The block/virtio_net config/enum shape is taken from a real M0 checkpoint.
// These tests isolate outer validation; the runner still does typed validation.
fn valid_state(machine: &ProbeVmConfig) -> serde_json::Value {
    json!({
        "header":{"format_epoch":1}, "vm_kvm_state":{}, "vcpu_states":[{"id":0}],
        "memory_state":{"regions":[{"guest_addr":0,"size":1048576,"file_offset":0}]},
        "agentenv_probe":{"probe_only":true,"identity":agentenv_dragonball_runner::build_identity(),"machine":machine},
        "device_states":{
            "balloon":null,
            "block":{"devices": machine.block_devices.iter().map(|disk| json!({
                "config":{
                    "device_type":"RawBlock", "drive_id":disk.drive_id,
                    "path_on_host":disk.path_on_host, "is_root_device":disk.is_root,
                    "is_read_only":disk.read_only, "is_direct":!(disk.is_root && disk.read_only),
                    "is_volume_slot":false,"backing_read_only":null,
                    "no_drop":false,"num_queues":1,"part_uuid":null,
                    "queue_size":256,"rate_limiter":null,"sparse":false,"use_generic_irq":false,
                    "use_shared_irq":false,"use_pci_bus":false
                },
                "device_info":{"RawBlock":{}},"transport":{"Mmio":{}}
            })).collect::<Vec<_>>()},
            "virtio_net":{"devices":[{
                "config":{
                    "backend":{"type":"virtio","iface_id":machine.network.iface_id,"host_dev_name":machine.network.tap_name,
                        "allow_duplicate_mac":false,"rx_rate_limiter":null,"tx_rate_limiter":null},
                    "guest_mac":null,"num_queues":null,"queue_size":null,
                    "use_generic_irq":false,"use_shared_irq":false
                },
                "device_info":{},"transport":{"Mmio":{}}
            }]}
        }
    })
}

#[test]
fn probe_accepts_empty_balloon_manager_but_not_balloon_devices() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let memory = dir.path().join("memory.bin");
    std::fs::File::create(&memory)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    let machine = config();
    let mut value = valid_state(&machine);
    // The pinned Kata save_state always serializes the compiled-in manager,
    // even when the M0 profile did not attach a balloon device.
    value["device_states"]["balloon"] = json!({"devices":[]});
    std::fs::write(&state, value.to_string()).unwrap();
    validate_snapshot(&state, &memory, &machine).unwrap();
    for invalid in [
        json!({"devices":[{}]}),
        json!({}),
        json!({"devices":null}),
        json!({"devices":[], "unknown":true}),
    ] {
        value["device_states"]["balloon"] = invalid;
        std::fs::write(&state, value.to_string()).unwrap();
        assert!(validate_snapshot(&state, &memory, &machine).is_err());
    }
}

#[test]
fn saved_device_membership_and_config_must_match_machine_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let memory = dir.path().join("memory.bin");
    std::fs::File::create(&memory)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    let machine = config();
    let valid = valid_state(&machine);
    for (pointer, replacement) in [
        ("/device_states/block", json!(null)),
        ("/device_states/virtio_net", json!(null)),
        ("/device_states/block/devices", json!([])),
        ("/device_states/virtio_net/devices", json!([])),
        (
            "/device_states/block/devices/0/config/drive_id",
            json!("foreign"),
        ),
        (
            "/device_states/block/devices/0/config/path_on_host",
            json!("/foreign"),
        ),
        (
            "/device_states/block/devices/0/config/is_read_only",
            json!(false),
        ),
        (
            "/device_states/block/devices/0/config/is_root_device",
            json!(false),
        ),
        (
            "/device_states/block/devices/0/config/is_direct",
            json!(true),
        ),
        ("/device_states/block/devices/0/config/no_drop", json!(true)),
        (
            "/device_states/block/devices/0/config/use_pci_bus",
            json!(true),
        ),
        (
            "/device_states/virtio_net/devices/0/config/backend/iface_id",
            json!("foreign"),
        ),
        (
            "/device_states/virtio_net/devices/0/config/backend/host_dev_name",
            json!("foreign"),
        ),
    ] {
        let mut broken = valid.clone();
        *broken.pointer_mut(pointer).unwrap() = replacement;
        std::fs::write(&state, broken.to_string()).unwrap();
        assert!(
            validate_snapshot(&state, &memory, &machine).is_err(),
            "accepted changed saved devices at {pointer}"
        );
    }
    let mut machine = machine;
    machine.block_devices.push(ProbeBlockDevice {
        drive_id: "user".into(),
        path_on_host: "/user".into(),
        read_only: false,
        is_root: false,
    });
    let mut reordered = valid_state(&machine);
    reordered["device_states"]["block"]["devices"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    std::fs::write(&state, reordered.to_string()).unwrap();
    assert!(
        validate_snapshot(&state, &memory, &machine).is_err(),
        "accepted reordered block devices"
    );
}
