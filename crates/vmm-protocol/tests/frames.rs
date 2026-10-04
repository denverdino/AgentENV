use std::io::{Cursor, Read};

use agentenv_vmm_protocol::{
    decode_request, decode_response, encode_frame, encode_response, read_request,
    validate_response, ProbeCommand, ProbeReply, RequestEnvelope, ResponseEnvelope,
};

const HELLO: &[u8] = br#"{"protocol_version":1,"request_id":42,"sandbox_instance_id":"probe-instance-1","budget_ms":4096,"command":{"op":"hello"}}"#;

fn raw_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    frame
}

fn hello_request() -> RequestEnvelope {
    RequestEnvelope {
        protocol_version: 1,
        request_id: 42,
        sandbox_instance_id: "probe-instance-1".into(),
        budget_ms: 4096,
        command: ProbeCommand::Hello,
    }
}

#[test]
fn request_roundtrips_with_instance_identity() {
    // Dropping an identity or request field can misroute a VMM operation.
    let request = RequestEnvelope {
        protocol_version: 1,
        request_id: 42,
        sandbox_instance_id: "probe-instance-1".into(),
        budget_ms: 4096,
        command: ProbeCommand::Hello,
    };
    let frame = encode_frame(&request).unwrap();
    assert_eq!(decode_request(&frame).unwrap(), request);
    let body: serde_json::Value = serde_json::from_slice(&frame[4..]).unwrap();
    assert_eq!(body["protocol_version"], 1);
    assert_eq!(body["request_id"], 42);
    assert_eq!(body["sandbox_instance_id"], "probe-instance-1");
    assert_eq!(body["budget_ms"], 4096);
    assert_eq!(body["command"], serde_json::json!({"op": "hello"}));
}

#[test]
fn truncated_frame_is_rejected() {
    for length in 0..4 {
        assert!(decode_request(&[0; 4][..length]).is_err());
    }
    let mut frame = raw_frame(HELLO);
    frame.pop();
    assert!(decode_request(&frame).is_err());
    assert!(read_request(&mut Cursor::new(frame)).is_err());
}

#[test]
fn oversized_frame_is_rejected_before_allocation() {
    struct HeaderOnly(Cursor<[u8; 4]>);
    impl Read for HeaderOnly {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                self.0.position() < 4,
                "oversized frame attempted a body read"
            );
            self.0.read(buffer)
        }
    }
    let header = 1_048_577_u32.to_be_bytes();
    assert!(read_request(&mut HeaderOnly(Cursor::new(header))).is_err());
}

#[test]
fn unknown_protocol_version_is_rejected() {
    let mut request = hello_request();
    request.protocol_version = 2;
    let frame = raw_frame(&serde_json::to_vec(&request).unwrap());
    assert!(decode_request(&frame).is_err());
    assert!(encode_frame(&request).is_err());
}

#[test]
fn empty_instance_and_zero_budget_are_rejected() {
    for request in [
        RequestEnvelope {
            sandbox_instance_id: String::new(),
            ..hello_request()
        },
        RequestEnvelope {
            budget_ms: 0,
            ..hello_request()
        },
    ] {
        assert!(decode_request(&raw_frame(&serde_json::to_vec(&request).unwrap())).is_err());
        assert!(encode_frame(&request).is_err());
    }
}

#[test]
fn response_from_other_instance_is_rejected() {
    let response = ResponseEnvelope {
        protocol_version: 1,
        request_id: 42,
        sandbox_instance_id: "other-instance".into(),
        result: Ok(ProbeReply::Done),
    };
    assert!(validate_response(&hello_request(), &response).is_err());
}

#[test]
fn response_with_wrong_request_or_version_is_rejected() {
    for (version, request_id) in [(1, 43), (2, 42)] {
        let response = ResponseEnvelope {
            protocol_version: version,
            request_id,
            sandbox_instance_id: "probe-instance-1".into(),
            result: Ok(ProbeReply::Done),
        };
        assert!(validate_response(&hello_request(), &response).is_err());
    }
}

#[test]
fn trailing_frame_bytes_are_rejected() {
    let mut frame = raw_frame(HELLO);
    frame.push(0);
    assert!(decode_request(&frame).is_err());
}

#[test]
fn largest_legal_body_and_consecutive_stream_frames_are_accepted() {
    // JSON permits whitespace; padding gives an independently sized exact boundary.
    let mut body = HELLO.to_vec();
    body.resize(1_048_576, b' ');
    let frame = raw_frame(&body);
    assert_eq!(frame.len(), 1_048_580);
    assert_eq!(decode_request(&frame).unwrap(), hello_request());
    let mut stream = Cursor::new([frame, raw_frame(HELLO)].concat());
    assert_eq!(read_request(&mut stream).unwrap(), hello_request());
    assert_eq!(read_request(&mut stream).unwrap(), hello_request());
    assert_eq!(stream.position(), stream.get_ref().len() as u64);
}

#[test]
fn response_roundtrips_and_retains_request_identity() {
    let response = ResponseEnvelope {
        protocol_version: 1,
        request_id: 42,
        sandbox_instance_id: "probe-instance-1".into(),
        result: Ok(ProbeReply::Done),
    };
    let frame = encode_response(&response).unwrap();
    assert_eq!(decode_response(&frame).unwrap(), response);
    validate_response(&hello_request(), &response).unwrap();
}

#[test]
fn oversized_encoded_command_and_response_are_rejected() {
    let request = RequestEnvelope {
        command: ProbeCommand::Configure(agentenv_vmm_protocol::ProbeVmConfig {
            vcpu_count: 1,
            memory_mib: 128,
            kernel_path: "/test/kernel".into(),
            boot_args: "x".repeat(1_048_576),
            block_devices: Vec::new(),
            network: agentenv_vmm_protocol::ProbeNetworkDevice {
                iface_id: "eth0".into(),
                tap_name: "tap0".into(),
            },
        }),
        ..hello_request()
    };
    assert!(encode_frame(&request).is_err());
    let response = ResponseEnvelope {
        protocol_version: 1,
        request_id: 42,
        sandbox_instance_id: "probe-instance-1".into(),
        result: Err(agentenv_vmm_protocol::ProbeError {
            code: "test_error".into(),
            message: "x".repeat(1_048_576),
        }),
    };
    assert!(encode_response(&response).is_err());
}

#[test]
fn invalid_response_identity_and_unknown_fields_are_rejected() {
    for (version, instance_id) in [(2, "probe-instance-1"), (1, "")] {
        let response = ResponseEnvelope {
            protocol_version: version,
            request_id: 42,
            sandbox_instance_id: instance_id.into(),
            result: Ok(ProbeReply::Done),
        };
        assert!(encode_response(&response).is_err());
        assert!(decode_response(&raw_frame(&serde_json::to_vec(&response).unwrap())).is_err());
    }
    let mut value: serde_json::Value = serde_json::from_slice(HELLO).unwrap();
    value["unexpected"] = serde_json::json!(true);
    assert!(decode_request(&raw_frame(&serde_json::to_vec(&value).unwrap())).is_err());
}
