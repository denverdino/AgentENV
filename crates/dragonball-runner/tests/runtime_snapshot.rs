#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use agentenv_dragonball_runner::runtime_snapshot::{capture_envelope, restore_state};
use agentenv_vmm_protocol::runtime::*;
use serde_json::{json, Value};

fn machine() -> RuntimeVmConfig {
    RuntimeVmConfig {
        vcpu_count: 1,
        memory_mib: 16,
        kernel_path: "/source/kernel".into(),
        boot_args: "console=ttyS0".into(),
        block_devices: vec![RuntimeBlockDevice {
            drive_id: "root".into(),
            path_on_host: "/dev/ublkb17".into(),
            read_only: false,
            is_root: true,
            is_volume_slot: false,
            backing_read_only: None,
            rate_limiter: None,
        }],
        network: RuntimeNetworkDevice {
            iface_id: "eth0".into(),
            tap_name: "old-tap".into(),
        },
        identity: agentenv_dragonball_runner::runtime_service::restore_identity(
            "6.1.175".into(),
            "0.1.0".into(),
            "0.6.0".into(),
        ),
    }
}

#[test]
fn tools_snapshot_preserves_buffered_reads_and_rejects_wrong_io_policy() {
    for (read_only, direct) in [(true, false), (false, true)] {
        let mut source = machine();
        source.block_devices[0].read_only = read_only;
        let mut raw = raw_state();
        let block = &mut raw["device_states"]["block"]["devices"][0];
        block["config"]["is_read_only"] = json!(read_only);
        block["config"]["is_direct"] = json!(direct);
        block["device_info"]["RawBlock"]["avail_features"] =
            json!(block_profile_features(read_only));
        let envelope = capture_envelope(raw.clone(), &source, 16 * 1024 * 1024).unwrap();
        let restored = restore_state(&envelope, &source, 16 * 1024 * 1024).unwrap();
        assert_eq!(
            restored["device_states"]["block"],
            raw["device_states"]["block"]
        );
        raw["device_states"]["block"]["devices"][0]["config"]["is_direct"] = json!(!direct);
        assert!(capture_envelope(raw, &source, 16 * 1024 * 1024).is_err());
        let mut wrong = envelope;
        wrong.upstream_state["device_states"]["block"]["devices"][0]["config"]["is_direct"] =
            json!(!direct);
        assert!(wrong.validate_state_topology().is_err());
        assert!(restore_state(&wrong, &source, 16 * 1024 * 1024).is_err());
    }
}

#[test]
fn runtime_snapshot_preserves_then_replaces_only_host_rate_limits() {
    let captured_rate = json!({
        "bandwidth":{"size":1048576,"one_time_burst":2097152,"refill_time":1000},
        "ops":{"size":100,"one_time_burst":0,"refill_time":1000}
    });
    let mut config = serde_json::to_value(machine()).unwrap();
    config["block_devices"][0]["rate_limiter"] = captured_rate.clone();
    let source: RuntimeVmConfig = serde_json::from_value(config.clone())
        .expect("runtime must accept the checked current disk rate policy");
    let mut raw = raw_state();
    raw["device_states"]["block"]["devices"][0]["config"]["rate_limiter"] = captured_rate.clone();
    let envelope = capture_envelope(raw, &source, 16 * 1024 * 1024).unwrap();
    assert_eq!(
        serde_json::to_value(&envelope.machine.block_devices[0]).unwrap()["rate_limiter"],
        captured_rate
    );

    for current_rate in [
        serde_json::Value::Null,
        json!({
            "bandwidth":{"size":4194304,"one_time_burst":0,"refill_time":1000},
            "ops":{"size":0,"one_time_burst":0,"refill_time":0}
        }),
    ] {
        config["block_devices"][0]["rate_limiter"] = current_rate.clone();
        let current: RuntimeVmConfig = serde_json::from_value(config.clone()).unwrap();
        let restored = restore_state(&envelope, &current, 16 * 1024 * 1024).unwrap();
        let mut expected = envelope.upstream_state.clone();
        expected["device_states"]["block"]["devices"][0]["config"]["path_on_host"] =
            json!("/dev/ublkb17");
        expected["device_states"]["block"]["devices"][0]["config"]["rate_limiter"] = current_rate;
        expected["device_states"]["virtio_net"]["devices"][0]["config"]["backend"]
            ["host_dev_name"] = json!("old-tap");
        assert_eq!(
            restored, expected,
            "only checked host bindings/rate policy may change"
        );
    }
    let mut forged = serde_json::to_value(&envelope).unwrap();
    forged["machine"]["block_devices"][0]["rate_limiter"] = serde_json::Value::Null;
    let forged = serde_json::from_value(forged).unwrap();
    assert!(restore_state(&forged, &source, 16 * 1024 * 1024).is_err());
}

// A complete typed serialization fixture, not a claim of guest execution.
fn raw_state() -> Value {
    use dragonball::snapshot::{MicrovmState, VmKvmState};
    let state = MicrovmState {
        vm_kvm_state: Some(VmKvmState {
            pit: Default::default(),
            clock: Default::default(),
            pic_master: Default::default(),
            pic_slave: Default::default(),
            ioapic: Default::default(),
        }),
        vcpu_states: vec![dragonball::vcpu::VcpuState {
            id: 0,
            cpuid: vmm_sys_util::fam::FamStructWrapper::new(0).unwrap(),
            msrs: vec![],
            regs: Default::default(),
            sregs: Default::default(),
            xsave: Default::default(),
            xcrs: Default::default(),
            debug_regs: Default::default(),
            lapic: Default::default(),
            mp_state: Default::default(),
            vcpu_events: Default::default(),
            tsc_khz: None,
        }],
        memory_state: Some(dragonball::address_space_manager::GuestMemoryState {
            regions: vec![dragonball::address_space_manager::GuestMemoryRegionState {
                guest_addr: 0,
                size: 16 * 1024 * 1024,
                file_offset: 0,
            }],
        }),
        capture_cpu: Some(dragonball::snapshot::capture::CpuRequirements {
            cpuid: vmm_sys_util::fam::FamStructWrapper::new(0).unwrap(),
            msr_indices: vec![],
        }),
        ..Default::default()
    };
    let mut raw = serde_json::to_value(state).unwrap();
    raw["device_states"]["block"] = json!({"devices":[{
        "config": {"drive_id":"root", "device_type":"RawBlock", "path_on_host":"/dev/ublkb17", "is_root_device":true, "part_uuid":null, "is_read_only":false, "is_direct":true, "no_drop":false, "sparse":false, "num_queues":1, "queue_size":256, "rate_limiter":null, "use_shared_irq":false, "use_generic_irq":false, "use_pci_bus":false},
        "device_info":{"RawBlock":{"avail_features":(1u64<<32)|(1u64<<1)|(1u64<<2),"acked_features":0,"config_space":block_space(4096)}},
        "transport":{"Mmio": transport(1)}
    }]});
    raw["device_states"]["virtio_net"] = json!({"devices":[{
        "config":{"num_queues":null,"queue_size":null,"backend":{"type":"virtio","iface_id":"eth0","host_dev_name":"old-tap","rx_rate_limiter":null,"tx_rate_limiter":null,"allow_duplicate_mac":false},"guest_mac":null,"use_shared_irq":false,"use_generic_irq":false},
        "device_info":{"avail_features":(1u64<<32)|(1u64<<0)|(1u64<<1)|(1u64<<7)|(1u64<<10)|(1u64<<11)|(1u64<<14),"acked_features":0,"config_space":[0,1,2,3,4,5,0,0,0,0,220,5]},
        "transport":{"Mmio": transport(2)}
    }]});
    let mut balloon_transport = transport(3);
    for queue in balloon_transport["queues"].as_array_mut().unwrap() {
        queue["queue"]["max_size"] = json!(128);
        queue["queue"]["size"] = json!(128);
    }
    raw["device_states"]["balloon"] = json!({"devices":[{
        "config":{"balloon_id":"memory","size_mib":0,"use_shared_irq":false,"use_generic_irq":false,"f_deflate_on_oom":true,"f_reporting":true},
        "device_info":{"device_info":{"avail_features":(1u64<<32)|(1u64<<2)|(1u64<<5),"acked_features":(1u64<<32)|(1u64<<2)|(1u64<<5),"config_space":[0,0,0,0,0,0,0,0]},"num_pages":0,"actual":0},
        "transport":{"Mmio":balloon_transport}
    }]});
    // Canonicalize only our known fixture, never a production input.
    serde_json::to_value(serde_json::from_value::<MicrovmState>(raw).unwrap()).unwrap()
}

#[test]
fn m2_snapshot_requires_exact_balloon_profile_and_preserves_state() {
    let raw = raw_state();
    let source = machine();
    let envelope = capture_envelope(raw.clone(), &source, 16 * 1024 * 1024).unwrap();
    let restored = restore_state(&envelope, &source, 16 * 1024 * 1024).unwrap();
    assert_eq!(
        restored["device_states"]["balloon"],
        raw["device_states"]["balloon"]
    );
    for (pointer, value) in [
        ("/device_states/balloon", Value::Null),
        (
            "/device_states/balloon/devices/0/config/f_reporting",
            json!(false),
        ),
        (
            "/device_states/balloon/devices/0/config/balloon_id",
            json!("other"),
        ),
        (
            "/device_states/balloon/devices/0/device_info/device_info/avail_features",
            json!(1u64 << 32),
        ),
        (
            "/device_states/balloon/devices/0/device_info/num_pages",
            json!(1),
        ),
        (
            "/device_states/balloon/devices/0/transport/Mmio/queues/0/queue/max_size",
            json!(256),
        ),
    ] {
        let mut bad = raw.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            capture_envelope(bad, &source, 16 * 1024 * 1024).is_err(),
            "accepted {pointer}"
        );
    }
}

fn transport(count: usize) -> Value {
    json!({"driver_status":15,"config_generation":0,"interrupt_status":0,"features_select":0,"acked_features_select":0,"queue_select":0,
        "queues":(0..count).map(|index| json!({"index":index,"queue":{"max_size":256,"size":256,"ready":true,"desc_table":4096,"avail_ring":8192,"used_ring":12288,"next_avail":0,"next_used":0,"event_idx_enabled":false}})).collect::<Vec<_>>(),
        "device_activated":true,"msi_enabled":false,"msi_vectors":[]})
}

fn block_space(capacity: u64) -> Vec<u8> {
    let mut space = vec![0; 64];
    space[..8].copy_from_slice(&(capacity / 512).to_le_bytes());
    space[8..12].copy_from_slice(&0x100000u32.to_le_bytes());
    space[12..16].copy_from_slice(&16u32.to_le_bytes());
    space[34..36].copy_from_slice(&1u16.to_le_bytes());
    space
}

#[test]
fn runtime_envelope_contains_no_node_local_bindings() {
    let source = machine();
    let raw = raw_state();
    let envelope = capture_envelope(raw.clone(), &source, 16 * 1024 * 1024).unwrap();
    let text = serde_json::to_string(&envelope).unwrap();
    for forbidden in [
        "/dev/ublkb17",
        "old-tap",
        "/source/kernel",
        "runner_instance_id",
        "host_address",
        "host_base",
        "changed_ranges",
        "zero_ranges",
        "netns",
    ] {
        assert!(!text.contains(forbidden), "persisted {forbidden}");
    }
    let mut destination = source.clone();
    destination.block_devices[0].path_on_host = "/current/disk".into();
    destination.network.tap_name = "new-tap".into();
    let restored = restore_state(&envelope, &destination, 16 * 1024 * 1024).unwrap();
    assert_eq!(
        restored["device_states"]["block"]["devices"][0]["config"]["path_on_host"],
        "/current/disk"
    );
    assert_eq!(
        restored["device_states"]["virtio_net"]["devices"][0]["config"]["backend"]["host_dev_name"],
        "new-tap"
    );
    for key in ["device_info", "transport"] {
        assert_eq!(
            restored["device_states"]["block"]["devices"][0][key],
            raw["device_states"]["block"]["devices"][0][key]
        );
        assert_eq!(
            restored["device_states"]["virtio_net"]["devices"][0][key],
            raw["device_states"]["virtio_net"]["devices"][0][key]
        );
    }
    assert_eq!(restored["vcpu_states"], raw["vcpu_states"]);
}

#[test]
fn runtime_envelope_rejects_identity_topology_and_short_memory() {
    let source = machine();
    let envelope = capture_envelope(raw_state(), &source, 16 * 1024 * 1024).unwrap();
    for field in [
        "backend",
        "architecture",
        "virtualization_mode",
        "runner_build_id",
        "dragonball_revision",
        "restore_abi_id",
        "cpu_policy_id",
        "device_profile_id",
        "memory_layout_id",
        "kernel_version",
        "tools_drive_version",
        "envd_version",
    ] {
        let mut value = serde_json::to_value(&envelope).unwrap();
        value["identity"][field] = json!("different");
        let input: RuntimeSnapshotEnvelope = serde_json::from_value(value).unwrap();
        assert!(
            restore_state(&input, &source, 16 * 1024 * 1024).is_err(),
            "accepted {field}"
        );
    }
    let mut bad = envelope.clone();
    bad.identity.snapshot_format_epoch += 1;
    assert!(restore_state(&bad, &source, 16 * 1024 * 1024).is_err());
    bad = envelope.clone();
    bad.machine
        .block_devices
        .push(bad.machine.block_devices[0].clone());
    assert!(restore_state(&bad, &source, 16 * 1024 * 1024).is_err());
    bad = envelope.clone();
    bad.machine.network_id = "unknown".into();
    assert!(restore_state(&bad, &source, 16 * 1024 * 1024).is_err());
    assert!(restore_state(&envelope, &source, 4096).is_err());
    let mut escaped = source.clone();
    escaped.block_devices[0].path_on_host = "/current/../disk".into();
    assert!(restore_state(&envelope, &escaped, 16 * 1024 * 1024).is_err());
}

#[test]
fn runtime_envelope_rejects_unknown_state_fields_and_device_tampering() {
    let source = machine();
    for pointer in [
        "/pid",
        "/vcpu_states/0/host_address",
        "/device_states/writer",
        "/device_states/block/devices/0/config/unknown",
        "/device_states/block/devices/0/device_info/RawBlock/unknown",
        "/device_states/block/devices/0/transport/Mmio/unknown",
    ] {
        let mut raw = raw_state();
        let (parent, name) = pointer.rsplit_once('/').unwrap();
        let object = if parent.is_empty() {
            &mut raw
        } else {
            raw.pointer_mut(parent).unwrap()
        };
        object[name] = json!(123);
        assert!(
            capture_envelope(raw, &source, 16 * 1024 * 1024).is_err(),
            "accepted {pointer}"
        );
    }
    let envelope = capture_envelope(raw_state(), &source, 16 * 1024 * 1024).unwrap();
    for (pointer, value) in [
        ("/header/format_epoch", json!(2)),
        ("/vcpu_states/0/id", json!(1)),
        ("/memory_state/regions/0/file_offset", json!(4096)),
        (
            "/device_states/block/devices/0/config/is_read_only",
            json!(true),
        ),
        (
            "/device_states/block/devices/0/config/is_root_device",
            json!(false),
        ),
        ("/device_states/block/devices/0/config/num_queues", json!(2)),
        (
            "/device_states/block/devices/0/config/queue_size",
            json!(128),
        ),
        (
            "/device_states/block/devices/0/config/path_on_host",
            json!("/dev/ublkb9"),
        ),
        (
            "/device_states/virtio_net/devices/0/config/backend/host_dev_name",
            json!("old-tap"),
        ),
        (
            "/device_states/block/devices/0/transport/Mmio/device_activated",
            json!(false),
        ),
        (
            "/device_states/block/devices/0/device_info/RawBlock/acked_features",
            json!(1u64 << 63),
        ),
    ] {
        let mut bad = envelope.clone();
        *bad.upstream_state.pointer_mut(pointer).unwrap() = value;
        assert!(
            restore_state(&bad, &source, 16 * 1024 * 1024).is_err(),
            "accepted {pointer}"
        );
    }
    let mut bad = envelope;
    bad.memory_layout.regions[0].length = u64::MAX;
    assert!(restore_state(&bad, &source, 16 * 1024 * 1024).is_err());
}

#[test]
fn runtime_envelope_rejects_coordinated_ram_gpa_change() {
    let source = machine();
    let mut bad = capture_envelope(raw_state(), &source, 16 * 1024 * 1024).unwrap();
    bad.memory_layout.regions[0].guest_base = 4096;
    bad.upstream_state["memory_state"]["regions"][0]["guest_addr"] = json!(4096);
    assert!(
        restore_state(&bad, &source, 16 * 1024 * 1024).is_err(),
        "matching envelope/state still needs the qualified machine's GPA geometry"
    );
}

#[test]
fn runtime_envelope_rejects_device_profile_features_and_config_space_before_vmm_action() {
    let source = machine();
    let envelope = capture_envelope(raw_state(), &source, 16 * 1024 * 1024).unwrap();
    for pointer in [
        "/device_states/block/devices/0/device_info/RawBlock",
        "/device_states/virtio_net/devices/0/device_info",
    ] {
        let mut bad = envelope.clone();
        let info = bad.upstream_state.pointer_mut(pointer).unwrap();
        info["avail_features"] = json!(1u64 << 63);
        info["acked_features"] = json!(1u64 << 63);
        assert!(
            restore_state(&bad, &source, 16 * 1024 * 1024).is_err(),
            "accepted unknown offered+acked features: {pointer}"
        );
    }
    for index in [8usize, 12, 34, 36, 63] {
        let mut bad = envelope.clone();
        bad.upstream_state["device_states"]["block"]["devices"][0]["device_info"]["RawBlock"]
            ["config_space"][index] = json!(99);
        assert!(
            restore_state(&bad, &source, 16 * 1024 * 1024).is_err(),
            "accepted block config-space byte{index}"
        );
    }
}

#[test]
fn runtime_envelope_rebinds_disks_by_identity_not_position() {
    let mut source = machine();
    source.block_devices.push(RuntimeBlockDevice {
        drive_id: "extra-0".into(),
        path_on_host: "/source/extra".into(),
        read_only: true,
        is_root: false,
        is_volume_slot: false,
        backing_read_only: None,
        rate_limiter: None,
    });
    let mut raw = raw_state();
    let mut extra = raw["device_states"]["block"]["devices"][0].clone();
    extra["config"]["drive_id"] = json!("extra-0");
    extra["config"]["path_on_host"] = json!("/source/extra");
    extra["config"]["is_root_device"] = json!(false);
    extra["config"]["is_read_only"] = json!(true);
    extra["device_info"]["RawBlock"]["avail_features"] =
        json!((1u64 << 32) | (1u64 << 1) | (1u64 << 2) | (1u64 << 5));
    extra["device_info"]["RawBlock"]["config_space"] = json!(block_space(8192));
    raw["device_states"]["block"]["devices"]
        .as_array_mut()
        .unwrap()
        .push(extra);
    let envelope = capture_envelope(raw, &source, 16 * 1024 * 1024).unwrap();
    let mut destination = source;
    destination.block_devices[0].path_on_host = "/current/root".into();
    destination.block_devices[1].path_on_host = "/current/extra".into();
    destination.block_devices.swap(0, 1);
    let restored = restore_state(&envelope, &destination, 16 * 1024 * 1024).unwrap();
    let disks = restored["device_states"]["block"]["devices"]
        .as_array()
        .unwrap();
    assert_eq!(
        disks
            .iter()
            .find(|disk| disk["config"]["drive_id"] == "root")
            .unwrap()["config"]["path_on_host"],
        "/current/root"
    );
    assert_eq!(
        disks
            .iter()
            .find(|disk| disk["config"]["drive_id"] == "extra-0")
            .unwrap()["config"]["path_on_host"],
        "/current/extra"
    );
}

#[test]
fn runtime_slot_binding_keeps_opaque_features_and_captured_capacity() {
    let mut source = machine();
    source.block_devices.push(RuntimeBlockDevice {
        drive_id: "slot-0".into(),
        path_on_host: "/source/placeholder".into(),
        read_only: false,
        is_root: false,
        is_volume_slot: true,
        backing_read_only: Some(false),
        rate_limiter: None,
    });
    let mut raw = raw_state();
    let mut slot = raw["device_states"]["block"]["devices"][0].clone();
    slot["config"]["drive_id"] = json!("slot-0");
    slot["config"]["path_on_host"] = json!("/source/placeholder");
    slot["config"]["is_root_device"] = json!(false);
    slot["config"]["is_volume_slot"] = json!(true);
    slot["config"]["backing_read_only"] = json!(false);
    slot["device_info"]["RawBlock"]["config_space"] = json!(block_space(4096));
    raw["device_states"]["block"]["devices"]
        .as_array_mut()
        .unwrap()
        .push(slot.clone());
    let envelope = capture_envelope(raw, &source, 16 << 20).unwrap();
    let mut current = source;
    current.block_devices[1].path_on_host = "/current/real-volume".into();
    current.block_devices[1].backing_read_only = Some(true);
    let restored = restore_state(&envelope, &current, 16 << 20).unwrap();
    let restored_slot = &restored["device_states"]["block"]["devices"][1];
    assert_eq!(restored_slot["device_info"], slot["device_info"]);
    assert_eq!(restored_slot["transport"], slot["transport"]);
    assert_eq!(
        restored_slot["config"]["backing_read_only"], false,
        "new access must travel in the checked binding, not forged saved state"
    );
    assert_eq!(
        restored_slot["config"]["path_on_host"],
        "/current/real-volume"
    );
    current.block_devices[1].is_volume_slot = false;
    current.block_devices[1].backing_read_only = None;
    assert!(restore_state(&envelope, &current, 16 << 20).is_err());
}
