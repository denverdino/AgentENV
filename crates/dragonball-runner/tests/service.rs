use agentenv_dragonball_runner::service::{ProbeVmm, RunnerService};
use agentenv_vmm_protocol::*;

#[derive(Default)]
struct FakeVmm {
    actions: Vec<&'static str>,
    fail_start: bool,
    fail_stop: bool,
}

impl ProbeVmm for FakeVmm {
    fn configure(&mut self, _: &ProbeVmConfig) -> anyhow::Result<()> {
        self.actions.push("configure");
        Ok(())
    }
    fn start(&mut self) -> anyhow::Result<()> {
        self.actions.push("start");
        anyhow::ensure!(!self.fail_start, "injected start failure");
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.actions.push("pause");
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.actions.push("resume");
        Ok(())
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.actions.push("stop");
        anyhow::ensure!(!self.fail_stop, "injected stop failure");
        Ok(())
    }
}

fn request(id: u64, command: ProbeCommand) -> RequestEnvelope {
    RequestEnvelope {
        protocol_version: 1,
        request_id: id,
        sandbox_instance_id: "instance-1".into(),
        budget_ms: 1000,
        command,
    }
}

fn config() -> ProbeVmConfig {
    ProbeVmConfig {
        vcpu_count: 1,
        memory_mib: 128,
        kernel_path: "/inputs/vmlinux".into(),
        boot_args: "console=ttyS0".into(),
        block_devices: vec![ProbeBlockDevice {
            drive_id: "tools".into(),
            path_on_host: "/inputs/tools".into(),
            read_only: true,
            is_root: true,
        }],
        network: ProbeNetworkDevice {
            iface_id: "eth0".into(),
            tap_name: "tap0".into(),
        },
    }
}

#[test]
fn wrong_instance_never_reaches_vmm() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    let mut req = request(1, ProbeCommand::Configure(config()));
    req.sandbox_instance_id = "instance-2".into();
    assert_eq!(s.handle(req).result.unwrap_err().code, "wrong_instance");
    assert!(s.vmm.actions.is_empty());
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
}

#[test]
fn second_start_does_not_boot_twice() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    assert!(s.handle(request(2, ProbeCommand::Start)).result.is_ok());
    assert_eq!(
        s.handle(request(3, ProbeCommand::Start))
            .result
            .unwrap_err()
            .code,
        "invalid_phase"
    );
    assert_eq!(s.vmm.actions, ["configure", "start"]);
}

#[test]
fn stop_is_idempotent() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    assert!(s.handle(request(1, ProbeCommand::Stop)).result.is_ok());
    assert!(s.handle(request(2, ProbeCommand::Stop)).result.is_ok());
    assert_eq!(s.vmm.actions, ["stop"]);
    assert_eq!(
        s.handle(request(3, ProbeCommand::Status)).result.unwrap(),
        ProbeReply::Status(ProbePhase::Stopped)
    );
}

#[test]
fn configure_rejects_duplicate_device_ids() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    let mut cfg = config();
    cfg.block_devices.push(cfg.block_devices[0].clone());
    assert_eq!(
        s.handle(request(1, ProbeCommand::Configure(cfg)))
            .result
            .unwrap_err()
            .code,
        "invalid_config"
    );
    assert!(s.vmm.actions.is_empty());
}

#[test]
fn replayed_requests_never_repeat_actions() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    assert!(s
        .handle(request(10, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    for id in [10, 9] {
        assert_eq!(
            s.handle(request(id, ProbeCommand::Start))
                .result
                .unwrap_err()
                .code,
            "replayed_request"
        );
    }
    assert_eq!(s.vmm.actions, ["configure"]);
}

#[test]
fn failed_start_cleans_up_and_never_claims_running() {
    let mut s = RunnerService::new(
        "instance-1".into(),
        FakeVmm {
            fail_start: true,
            ..Default::default()
        },
    );
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    assert_eq!(
        s.handle(request(2, ProbeCommand::Start))
            .result
            .unwrap_err()
            .code,
        "vmm_error"
    );
    assert_eq!(s.vmm.actions, ["configure", "start", "stop"]);
    assert_eq!(
        s.handle(request(3, ProbeCommand::Status)).result.unwrap(),
        ProbeReply::Status(ProbePhase::Failed)
    );
    assert!(s.handle(request(4, ProbeCommand::Start)).result.is_err());
}

#[test]
fn failed_stop_does_not_claim_clean_stop() {
    let mut s = RunnerService::new(
        "instance-1".into(),
        FakeVmm {
            fail_stop: true,
            ..Default::default()
        },
    );
    assert!(s.handle(request(1, ProbeCommand::Stop)).result.is_err());
    assert_eq!(
        s.handle(request(2, ProbeCommand::Status)).result.unwrap(),
        ProbeReply::Status(ProbePhase::Failed)
    );
}

#[test]
fn pause_resume_obey_phase_and_running_probe_cannot_save() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    assert!(s.handle(request(1, ProbeCommand::Pause)).result.is_err());
    assert!(s
        .handle(request(2, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    assert!(s.handle(request(3, ProbeCommand::Start)).result.is_ok());
    assert!(s.handle(request(4, ProbeCommand::Pause)).result.is_ok());
    assert_eq!(
        s.handle(request(5, ProbeCommand::Status)).result.unwrap(),
        ProbeReply::Status(ProbePhase::Paused)
    );
    assert!(s.handle(request(6, ProbeCommand::Resume)).result.is_ok());
    assert_eq!(
        s.handle(request(
            7,
            ProbeCommand::SaveQuiesced {
                state_path: "/out/state".into(),
                memory_path: "/out/mem".into()
            }
        ))
        .result
        .unwrap_err()
        .code,
        "invalid_phase"
    );
    assert_eq!(s.vmm.actions, ["configure", "start", "pause", "resume"]);
}

#[test]
fn invalid_envelope_is_rejected_without_actions() {
    let mut s = RunnerService::new("instance-1".into(), FakeVmm::default());
    let mut req = request(1, ProbeCommand::Start);
    req.protocol_version = 2;
    assert!(s.handle(req).result.is_err());
    assert!(s.vmm.actions.is_empty());
}

#[test]
fn startup_failure_joins_started_workers() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    struct PartialVmm {
        stopped: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
        exit: Option<mpsc::Sender<()>>,
    }
    impl ProbeVmm for PartialVmm {
        fn configure(&mut self, _: &ProbeVmConfig) -> anyhow::Result<()> {
            Ok(())
        }
        fn start(&mut self) -> anyhow::Result<()> {
            let (exit, wait) = mpsc::channel();
            let stopped = self.stopped.clone();
            self.worker = Some(std::thread::spawn(move || {
                wait.recv().unwrap();
                stopped.store(true, Ordering::Release);
            }));
            self.exit = Some(exit);
            anyhow::bail!("failure after worker creation")
        }
        fn pause(&mut self) -> anyhow::Result<()> {
            unreachable!()
        }
        fn resume(&mut self) -> anyhow::Result<()> {
            unreachable!()
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            if let Some(exit) = self.exit.take() {
                exit.send(()).unwrap();
            }
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
            Ok(())
        }
    }
    let stopped = Arc::new(AtomicBool::new(false));
    let mut s = RunnerService::new(
        "instance-1".into(),
        PartialVmm {
            stopped: stopped.clone(),
            worker: None,
            exit: None,
        },
    );
    assert!(s
        .handle(request(1, ProbeCommand::Configure(config())))
        .result
        .is_ok());
    assert!(s.handle(request(2, ProbeCommand::Start)).result.is_err());
    assert!(stopped.load(Ordering::Acquire));
    assert!(s.vmm.worker.is_none());
    assert!(s.vmm.exit.is_none());
}
