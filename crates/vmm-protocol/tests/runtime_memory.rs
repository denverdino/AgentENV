use agentenv_vmm_protocol::{runtime::*, runtime_memory::*, *};

fn fragmented_map() -> (FrozenMemoryMap, MemoryLayout) {
    let layout = MemoryLayout::for_runtime_memory(8192);
    let map = FrozenMemoryMap {
        token: CaptureToken {
            runner_instance_id: "runner".into(),
            generation: 9,
        },
        layout_id: "packed-ram-4k-v2".into(),
        page_size: 4096,
        regions: vec![
            FrozenMemoryRegion {
                guest_base: 0,
                host_base: 0x1_0000_0000,
                image_offset: 0,
                length: 3 << 30,
            },
            FrozenMemoryRegion {
                guest_base: 4 << 30,
                host_base: 0x4_0000_0000,
                image_offset: 3 << 30,
                length: 5 << 30,
            },
        ],
        changed_ranges: (0..32768)
            .map(|n| MemoryRange {
                image_offset: n * 8192,
                length: 4096,
            })
            .collect(),
        zero_ranges: (0..8192)
            .map(|n| MemoryRange {
                image_offset: n * 32768,
                length: 4096,
            })
            .collect(),
    };
    (map, layout)
}

#[test]
fn m2_map_pagination_reassembles_fragmented_eight_gib() {
    let (map, layout) = fragmented_map();
    assert!(serde_json::to_vec(&map.changed_ranges).unwrap().len() > MAX_FRAME_BODY);
    let pager = MemoryMapPager::new(map.clone()).unwrap();
    let mut assembler =
        MemoryMapAssembler::new(map.token.clone(), map.layout_id.clone(), layout).unwrap();
    let mut cursor = 0;
    let mut pages = 0;
    loop {
        let chunk = pager.page(cursor).unwrap();
        assert!(chunk.changed_ranges.len() + chunk.zero_ranges.len() <= 1024);
        assert!(serde_json::to_vec(&chunk).unwrap().len() < MAX_FRAME_BODY);
        let next = chunk.next_cursor;
        let rebuilt = assembler.push(chunk).unwrap();
        pages += 1;
        if let Some(next) = next {
            assert!(rebuilt.is_none());
            assert!(next > cursor);
            cursor = next;
        } else {
            assert_eq!(rebuilt.unwrap(), map);
            break;
        }
    }
    assert_eq!(pages, 40);
}

#[test]
fn m2_map_pagination_rejects_stale_cursor_and_wrong_layout() {
    let (map, layout) = fragmented_map();
    let pager = MemoryMapPager::new(map.clone()).unwrap();
    for bad_kind in 0..9 {
        let mut assembler =
            MemoryMapAssembler::new(map.token.clone(), map.layout_id.clone(), layout.clone())
                .unwrap();
        let first = pager.page(0).unwrap();
        let mut bad = first.clone();
        match bad_kind {
            0 => bad.cursor = 1024,
            1 => bad.layout_id = "another-layout".into(),
            2 => bad.token.generation += 1,
            3 => bad.next_cursor = None,
            4 => bad.next_cursor = Some(2048),
            5 => bad.regions[1].host_base = bad.regions[0].host_base,
            6 => bad.regions[1].guest_base = 3 << 30,
            7 => bad.changed_ranges.push(bad.changed_ranges[0].clone()),
            _ => bad.changed_ranges[1].image_offset = 0,
        }
        assert!(assembler.push(bad).is_err(), "invalid chunk {bad_kind}");
        assert!(
            assembler.push(first.clone()).unwrap().is_none(),
            "rejected chunk must not advance assembly"
        );
        assert!(assembler.push(first).is_err(), "repeated cursor");
        let mut second = pager.page(1024).unwrap();
        second.changed_ranges[0].image_offset = 0;
        assert!(assembler.push(second).is_err(), "cross-page overlap");
    }
    assert!(pager.page(1).is_err());
    assert!(pager.page(u64::MAX).is_err());
}

#[test]
fn m2_map_rejects_bad_ranges_and_unowned_zero_pages() {
    let (original, _) = fragmented_map();
    for bad_kind in 0..7 {
        let mut map = original.clone();
        match bad_kind {
            0 => map.zero_ranges[0].image_offset = 4096,
            1 => map.changed_ranges[0].length = 0,
            2 => map.changed_ranges[0].image_offset = 1,
            3 => map.changed_ranges[0].length = u64::MAX,
            4 => map.regions[0].host_base = u64::MAX - 4095,
            5 => map.regions[1].image_offset = 4 << 30,
            _ => map.token.runner_instance_id.clear(),
        }
        assert!(MemoryMapPager::new(map).is_err(), "invalid map {bad_kind}");
    }
}

#[test]
fn m2_memory_frames_match_token_cursor_and_bound_size() {
    let (map, _) = fragmented_map();
    let pager = MemoryMapPager::new(map.clone()).unwrap();
    let request = RuntimeRequest {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: 4,
        sandbox_instance_id: "instance".into(),
        budget_ms: 1000,
        command: RuntimeCommand::ExportMemoryMap {
            token: map.token.clone(),
            cursor: 1024,
        },
    };
    let response = |chunk| RuntimeResponse {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: 4,
        sandbox_instance_id: "instance".into(),
        result: Ok(RuntimeReply::MemoryMap(chunk)),
    };
    let good = response(pager.page(1024).unwrap());
    let decoded = decode_runtime_response(&encode_runtime_response(&good).unwrap()).unwrap();
    validate_runtime_response(&request, &decoded).unwrap();
    let mut bad = pager.page(1024).unwrap();
    bad.token.generation += 1;
    assert!(validate_runtime_response(&request, &response(bad)).is_err());
    assert!(validate_runtime_response(&request, &response(pager.page(0).unwrap())).is_err());
    let mut oversized = pager.page(1024).unwrap();
    oversized.changed_ranges = map.changed_ranges;
    assert!(matches!(
        encode_runtime_response(&response(oversized)),
        Err(ProtocolError::TooLarge(_))
    ));
}

#[test]
fn m2_runtime_status_cannot_claim_running_with_memory_pin() {
    let request = RuntimeRequest {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: 1,
        sandbox_instance_id: "instance".into(),
        budget_ms: 1000,
        command: RuntimeCommand::Status,
    };
    let response = RuntimeResponse {
        mode: "runtime".into(),
        protocol_version: RUNTIME_PROTOCOL_VERSION,
        request_id: 1,
        sandbox_instance_id: "instance".into(),
        result: Ok(RuntimeReply::Status(RuntimeStatus {
            phase: RuntimePhase::Running,
            capture: None,
            memory_map_pinned: true,
        })),
    };
    assert!(validate_runtime_response(&request, &response).is_err());
}

#[test]
fn m2_runtime_wire_rejects_m1_while_probe_stays_v1() {
    let mut request = RuntimeRequest {
        mode: "runtime".into(),
        protocol_version: 1,
        request_id: 1,
        sandbox_instance_id: "instance".into(),
        budget_ms: 1000,
        command: RuntimeCommand::Hello,
    };
    assert!(matches!(
        encode_runtime_request(&request),
        Err(ProtocolError::UnsupportedVersion(1))
    ));
    request.protocol_version = 2;
    assert_eq!(
        decode_runtime_request(&encode_runtime_request(&request).unwrap()).unwrap(),
        request
    );
    let probe = RequestEnvelope {
        protocol_version: 1,
        request_id: 1,
        sandbox_instance_id: "probe".into(),
        budget_ms: 1000,
        command: ProbeCommand::Hello,
    };
    assert_eq!(
        decode_request(&encode_frame(&probe).unwrap()).unwrap(),
        probe
    );
}
