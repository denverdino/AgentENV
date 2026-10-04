use agentenv_vmm_protocol::runtime::*;
use agentenv_vmm_protocol::*;

fn request() -> RuntimeRequest {
    RuntimeRequest {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: 7,
        sandbox_instance_id: "sandbox-1".into(),
        budget_ms: 1000,
        command: RuntimeCommand::Hello,
    }
}

#[test]
fn runtime_frames_are_separate_from_probe_frames() {
    let value = request();
    let bytes = encode_runtime_request(&value).unwrap();
    assert_eq!(decode_runtime_request(&bytes).unwrap(), value);
    assert!(decode_request(&bytes).is_err());
    let probe = RequestEnvelope {
        protocol_version: 1,
        request_id: 7,
        sandbox_instance_id: "sandbox-1".into(),
        budget_ms: 1000,
        command: ProbeCommand::Hello,
    };
    assert!(decode_runtime_request(&encode_frame(&probe).unwrap()).is_err());
}

#[test]
fn runtime_rejects_version_one_truncation_and_oversize() {
    let bytes = encode_runtime_request(&request()).unwrap();
    assert!(matches!(
        decode_runtime_request(&bytes[..bytes.len() - 1]),
        Err(ProtocolError::Truncated)
    ));
    let mut bad = request();
    bad.protocol_version = 1;
    assert!(matches!(
        encode_runtime_request(&bad),
        Err(ProtocolError::UnsupportedVersion(1))
    ));
    bad = request();
    bad.mode = "probe".into();
    assert!(encode_runtime_request(&bad).is_err());
    bad = request();
    bad.sandbox_instance_id = "x".repeat(MAX_FRAME_BODY);
    assert!(matches!(
        encode_runtime_request(&bad),
        Err(ProtocolError::TooLarge(_))
    ));
    assert!(matches!(
        decode_runtime_request(&((MAX_FRAME_BODY + 1) as u32).to_be_bytes()),
        Err(ProtocolError::TooLarge(_))
    ));
}

#[test]
fn runtime_response_requires_request_ownership() {
    let mut req = request();
    req.command = RuntimeCommand::Status;
    let mut reply = RuntimeResponse {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: req.request_id,
        sandbox_instance_id: req.sandbox_instance_id.clone(),
        result: Ok(RuntimeReply::Status(RuntimeStatus {
            phase: RuntimePhase::Unconfigured,
            capture: None,
            memory_map_pinned: false,
        })),
    };
    let bytes = encode_runtime_response(&reply).unwrap();
    assert_eq!(decode_runtime_response(&bytes).unwrap(), reply);
    validate_runtime_response(&req, &reply).unwrap();
    reply.request_id += 1;
    assert!(validate_runtime_response(&req, &reply).is_err());
    reply.request_id = req.request_id;
    reply.sandbox_instance_id = "neighbor".into();
    assert!(validate_runtime_response(&req, &reply).is_err());
}

#[test]
fn runtime_response_rejects_unrelated_success_kind() {
    let req = request();
    let reply = RuntimeResponse {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: req.request_id,
        sandbox_instance_id: req.sandbox_instance_id.clone(),
        result: Ok(RuntimeReply::Done),
    };
    assert!(validate_runtime_response(&req, &reply).is_err());
}

#[test]
fn runtime_status_cannot_claim_held_without_proof() {
    let mut req = request();
    req.command = RuntimeCommand::Status;
    let reply = RuntimeResponse {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: req.request_id,
        sandbox_instance_id: req.sandbox_instance_id.clone(),
        result: Ok(RuntimeReply::Status(RuntimeStatus {
            phase: RuntimePhase::Held,
            capture: None,
            memory_map_pinned: false,
        })),
    };
    assert!(validate_runtime_response(&req, &reply).is_err());
}

#[test]
fn runtime_held_report_requires_valid_token_cpu_and_worker_proof() {
    let mut req = request();
    req.command = RuntimeCommand::BeginCapture;
    let report = CaptureReport {
        token: CaptureToken {
            runner_instance_id: "runner".into(),
            generation: 1,
        },
        memory_layout: MemoryLayout {
            page_size: 4096,
            image_size: 4096,
            regions: vec![MemoryRegion {
                guest_base: 0,
                image_offset: 0,
                length: 4096,
            }],
        },
        vcpu_ids: vec![0],
        worker_acks: vec![
            WorkerAck {
                device_id: "balloon:memory".into(),
                generation: 1,
                pending_io: 0,
                memory_writers: 0,
                flush_completed: false,
            },
            WorkerAck {
                device_id: "block:root/q0".into(),
                generation: 1,
                pending_io: 0,
                memory_writers: 0,
                flush_completed: true,
            },
            WorkerAck {
                device_id: "net:eth0".into(),
                generation: 1,
                pending_io: 0,
                memory_writers: 0,
                flush_completed: false,
            },
        ],
    };
    let response = |report| RuntimeResponse {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: req.request_id,
        sandbox_instance_id: req.sandbox_instance_id.clone(),
        result: Ok(RuntimeReply::Held(report)),
    };
    validate_runtime_response(&req, &response(report.clone())).unwrap();
    let mut bad = report.clone();
    bad.token.generation = 0;
    assert!(validate_runtime_response(&req, &response(bad)).is_err());
    let mut bad = report.clone();
    bad.vcpu_ids.clear();
    assert!(validate_runtime_response(&req, &response(bad)).is_err());
    let mut bad = report.clone();
    bad.worker_acks[0].pending_io = 1;
    assert!(validate_runtime_response(&req, &response(bad)).is_err());
    let mut bad = report.clone();
    bad.worker_acks[1].flush_completed = false;
    assert!(validate_runtime_response(&req, &response(bad)).is_err());
    let mut bad = report.clone();
    bad.worker_acks[1] = bad.worker_acks[0].clone();
    assert!(validate_runtime_response(&req, &response(bad)).is_err());
    let mut bad = report;
    bad.memory_layout.image_size = 0;
    assert!(validate_runtime_response(&req, &response(bad)).is_err());
}

#[test]
fn runtime_layout_rejects_overflow_overlap_and_gpa_hole_padding() {
    let good = MemoryLayout {
        page_size: 4096,
        image_size: 8192,
        regions: vec![
            MemoryRegion {
                guest_base: 0,
                image_offset: 0,
                length: 4096,
            },
            MemoryRegion {
                guest_base: 8192,
                image_offset: 4096,
                length: 4096,
            },
        ],
    };
    good.validate().unwrap();
    let mut bad = good.clone();
    bad.regions[1].image_offset = 8192;
    assert!(bad.validate().is_err());
    bad = good.clone();
    bad.regions[1].guest_base = 0;
    assert!(bad.validate().is_err());
    bad = good;
    bad.regions[1].guest_base = u64::MAX - 4095;
    assert!(bad.validate().is_err());
}
