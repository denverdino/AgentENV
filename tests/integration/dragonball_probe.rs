use std::path::PathBuf;

use agentenv::{
    sandbox::{
        dragonball::{DragonballM0Probe, DragonballM0ProbeConfig},
        BaseSandboxNetworkPolicy, SandboxAccessTokenGenerator, SandboxMetadataDocument,
        SandboxNetworkEgressPolicy, SandboxNetworkPolicy,
    },
    types::SandboxId,
    virtualization::VirtualizationMode,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};

async fn fixture_config(deny_all: bool) -> Result<(DragonballM0ProbeConfig, String)> {
    anyhow::ensure!(
        std::env::var("AENV_DRAGONBALL_M0_ISOLATED").as_deref() == Ok("1"),
        "use the isolated M0 capability runner; never run against a live host network"
    );
    anyhow::ensure!(
        std::fs::read_link("/proc/thread-self/ns/net")?.to_string_lossy()
            != std::env::var("AENV_DRAGONBALL_M0_PARENT_NETNS")?,
        "M0 test process did not enter the disposable network namespace"
    );
    anyhow::ensure!(
        unsafe { libc::geteuid() } != 0,
        "build and run M0 tests as a non-root runtime user"
    );
    let config = super::common::setup_runtime_only().await;
    let asset_file = std::env::var("AENV_DRAGONBALL_M0_ASSETS")
        .context("prepare M0 assets outside the isolated test network namespace")?;
    let assets: Value = serde_json::from_slice(&std::fs::read(asset_file)?)?;
    let path = |key: &str| -> Result<PathBuf> {
        Ok(PathBuf::from(
            assets[key]
                .as_str()
                .with_context(|| format!("missing asset {key}"))?,
        ))
    };
    let id = SandboxId::new();
    let token = SandboxAccessTokenGenerator::new("dragonball-m0-qualification")?.generate(id);
    let mut extra = serde_json::Map::new();
    extra.insert("imageConfigs".into(), assets["image_configs"].clone());
    let metadata =
        SandboxMetadataDocument::new(id, "dragonball-m0-existing-guest").with_extra(extra);
    let probe = DragonballM0ProbeConfig {
        runner_path: path("runner_path")?,
        kernel_path: path("kernel_path")?,
        tools_path: path("tools_path")?,
        image_path: path("image_path")?,
        work_root: config.runtime_path.join("dragonball-m0"),
        vcpu_count: 1,
        memory_mib: 512,
        sandbox_instance_id: uuid::Uuid::now_v7().to_string(),
        metadata,
        envd_access_token: token,
        extra_drives: Vec::new(),
        network_policy: SandboxNetworkPolicy::new(
            true,
            if deny_all {
                BaseSandboxNetworkPolicy::Deny
            } else {
                BaseSandboxNetworkPolicy::Default
            },
            SandboxNetworkEgressPolicy::default(),
        ),
        virtualization_mode: VirtualizationMode::Kvm,
        cpu_policy: "host-default".into(),
    };
    Ok((probe, id.to_string()))
}

async fn start(deny_all: bool) -> Result<(DragonballM0Probe, String)> {
    let (config, id) = fixture_config(deny_all).await?;
    Ok((DragonballM0Probe::start(config).await?, id))
}

fn memory_checksum(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let len = file.read(&mut bytes)?;
        if len == 0 {
            break;
        }
        hash.update(&bytes[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[tokio::test]
#[ignore = "requires quiesced Linux x86_64 KVM/ublk guest in isolated namespace"]
async fn dragonball_probe_quiesced_full_restore() -> Result<()> {
    let (config, _) = fixture_config(false).await?;
    let mut source = DragonballM0Probe::start(config.clone()).await?;
    let executor = source.executor()?;
    let marker = uuid::Uuid::now_v7().to_string();
    let opts = agentenv::sandbox::ProcessOpts::new();
    let process=executor.start_process("/bin/sh", &["-ec","marker=$1; mkfifo /run/m0-gate; printf '%s' \"$marker\" > /run/m0-ready; read continuation < /run/m0-gate; printf '%s %s\\n' \"$marker\" \"$continuation\" > /run/m0-result","fixture",&marker],&opts).await?;
    let pid = process.pid();
    drop(process); // no live host control stream crosses the checkpoint
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let out = executor
                .run_command("/bin/sh", &["-ec", "cat /run/m0-ready"])
                .await?;
            if out.exit_code == 0 && out.stdout == marker {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await??;
    let disk = executor
        .run_command(
            "/bin/sh",
            &[
                "-ec",
                "test -b /dev/vda; test -b /dev/vdb; sha256sum /agentenv/envd; fsfreeze -f /",
            ],
        )
        .await?;
    assert_eq!(disk.exit_code, 0, "{}", disk.stderr);
    // No guest block writes after the filesystem freeze; the fixture waits on
    // a tmpfs FIFO, and all host command streams have completed before Pause.
    source.pause_probe().await?;
    let stats: Vec<_> = source
        .machine_config()?
        .block_devices
        .iter()
        .filter_map(|disk| disk.path_on_host.file_name())
        .filter_map(|name| name.to_str())
        .filter(|name| name.starts_with("ublkb"))
        .map(|name| PathBuf::from(format!("/sys/class/block/{name}/stat")))
        .collect();
    anyhow::ensure!(!stats.is_empty(), "fixture has no observed ublk backing");
    let mut previous = Vec::new();
    let mut stable = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current: Vec<_> = stats
                .iter()
                .map(std::fs::read_to_string)
                .collect::<std::io::Result<_>>()?;
            let idle = current
                .iter()
                .all(|stat| stat.split_whitespace().nth(8) == Some("0"));
            if idle && current == previous {
                stable += 1;
            } else {
                stable = 0;
            }
            if stable == 3 {
                return Ok::<_, anyhow::Error>(());
            }
            previous = current;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .context("fixture block IO did not reach observed idle/stable counters")??;
    let source_work = source.work_dir()?.to_owned();
    let artifacts = source
        .save_quiesced(&source_work.join("checkpoint"))
        .await?;
    let state: Value = serde_json::from_slice(&std::fs::read(&artifacts.state_path)?)?;
    // kvm_regs is serialized as its repr(C) bytes: 16 GPRs, RIP, RFLAGS.
    let regs = state["vcpu_states"][0]["regs"]
        .as_array()
        .context("saved kvm_regs bytes")?;
    anyhow::ensure!(regs.len() == 18 * 8, "unexpected saved kvm_regs size");
    let mut rip_bytes = [0u8; 8];
    for (byte, value) in rip_bytes.iter_mut().zip(&regs[16 * 8..17 * 8]) {
        *byte = u8::try_from(value.as_u64().context("invalid register byte")?)?;
    }
    let rip = u64::from_le_bytes(rip_bytes);
    anyhow::ensure!(rip != 0, "saved CPU instruction pointer is zero");
    let checksum = memory_checksum(&artifacts.memory_path)?;
    let evidence_dir = source_work
        .parent()
        .context("probe work root")?
        .join("evidence")
        .join(source_work.file_name().context("probe work name")?);
    std::fs::create_dir_all(&evidence_dir)?;
    std::fs::write(
        evidence_dir.join("fixture-io-evidence.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "block_stats": previous,
            "stable_samples": stable,
            "in_flight": 0,
            "rip": rip,
            "memory_sha256": checksum,
        }))?,
    )?;
    source.stop().await?;
    // Corrupt the actual typed checkpoint, leaving the outer machine stamp
    // unchanged. Each attempt must fail preflight, before any guest executes.
    for (pointer, replacement) in [
        ("/device_states/block", Value::Null),
        ("/device_states/virtio_net", Value::Null),
        ("/device_states/block/devices", serde_json::json!([])),
        (
            "/device_states/block/devices/0/config/drive_id",
            serde_json::json!("foreign"),
        ),
        (
            "/device_states/block/devices/0/config/path_on_host",
            serde_json::json!("/foreign/disk"),
        ),
        (
            "/device_states/virtio_net/devices/0/config/backend/iface_id",
            serde_json::json!("foreign"),
        ),
        (
            "/device_states/virtio_net/devices/0/config/backend/host_dev_name",
            serde_json::json!("foreign-tap"),
        ),
    ] {
        let mut corrupt = state.clone();
        *corrupt
            .pointer_mut(pointer)
            .context("checkpoint mutation target")? = replacement;
        std::fs::write(&artifacts.state_path, serde_json::to_vec(&corrupt)?)?;
        let (invalid_config, _) = fixture_config(false).await?;
        match DragonballM0Probe::restore_quiesced(invalid_config, &artifacts).await {
            Ok(mut unexpected) => {
                unexpected.stop().await?;
                anyhow::bail!("invalid saved device state accepted: {pointer}");
            }
            Err(error) => anyhow::ensure!(
                error.to_string().contains("snapshot_preflight_error"),
                "device mismatch was not rejected before restore: {pointer}: {error:#}"
            ),
        }
    }
    std::fs::write(&artifacts.state_path, serde_json::to_vec(&state)?)?;
    let (mut restored_config, restored_id) = fixture_config(false).await?;
    restored_config.metadata.snapshot_id = "dragonball-m0-restored".into();
    let mut restored = DragonballM0Probe::restore_quiesced(restored_config, &artifacts).await?;
    let command=format!("kill -0 {pid}; printf 'continued\\n' > /run/m0-gate; while ! test -f /run/m0-result; do sleep 0.01; done; cat /run/m0-result; sha256sum /agentenv/envd; printf '%s %s\\n' \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\"");
    let out = restored
        .executor()?
        .run_command_with_opts(
            "/bin/sh",
            &["-ec", &command],
            &agentenv::sandbox::ProcessOpts::new().with_timeout(std::time::Duration::from_secs(5)),
        )
        .await?;
    restored.stop().await?;
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert!(
        out.stdout.contains(&format!("{marker} continued")),
        "RAM marker / blocked CPU continuation missing"
    );
    assert!(
        out.stdout.contains(disk.stdout.trim()),
        "read-only tools contents changed"
    );
    assert!(
        out.stdout
            .contains(&format!("{restored_id} dragonball-m0-restored")),
        "restored metadata identity stale"
    );
    assert_eq!(
        memory_checksum(&artifacts.memory_path)?,
        checksum,
        "guest writes changed source memory"
    );
    drop(artifacts);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while source_work.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    eprintln!("quiesced restore: resumed PID {pid}, runtime RAM marker, tools digest, new metadata identity, private-memory checksum {checksum}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux x86_64 KVM/ublk/assets and an isolated outer network namespace"]
async fn dragonball_probe_cold_start_uses_existing_guest() -> Result<()> {
    let (mut probe, _) = start(false).await?;
    let output = probe.executor()?.run_command("/bin/sh", &["-ec", "test -b /dev/vda; test -b /dev/vdb; test -b /dev/vdc; test -x /agentenv/envd; findmnt -n -o SOURCE /; findmnt -n -o OPTIONS /agentenv; printf 'DRAGONBALL_GUEST_OK\\n'"]).await?;
    probe.stop().await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(output.stdout.contains("vdb"), "{}", output.stdout);
    assert!(output.stdout.contains("ro"), "{}", output.stdout);
    assert!(output.stdout.contains("DRAGONBALL_GUEST_OK"));
    eprintln!("guest storage/command evidence: {}", output.stdout.trim());
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux x86_64 KVM/ublk/assets and an isolated outer network namespace"]
async fn dragonball_probe_secure_token_and_metadata_match_instance() -> Result<()> {
    let (mut probe, id) = start(false).await?;
    let output = probe.executor()?.run_command("/bin/sh", &["-ec", "t=$(curl --fail --silent -X PUT -H X-metadata-token-ttl-seconds:60 http://169.254.169.254/latest/api/token); curl --fail --silent -H \"X-metadata-token: $t\" -H Accept:application/json http://169.254.169.254/; printf '\\n'; printf '%s %s\\n' \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\""]).await?;
    let mut wrong =
        envd::process::ProcessClient::connect(&probe.envd_address()?, Some("wrong-token")).await?;
    let result = wrong.list(envd::process::ListRequest {}).await;
    let mut wrong_http = envd::http_client::apis::configuration::Configuration::new();
    wrong_http.base_path = probe.envd_address()?;
    wrong_http.api_key = Some(envd::http_client::apis::configuration::ApiKey {
        prefix: None,
        key: "wrong-token".into(),
    });
    let http_result = envd::http_client::apis::default_api::envs_get(&wrong_http).await;
    probe.stop().await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    let doc: Value =
        serde_json::from_str(output.stdout.lines().next().context("metadata document")?)?;
    assert_eq!(doc["instanceID"], json!(id));
    assert_eq!(doc["envID"], json!("dragonball-m0-existing-guest"));
    assert!(doc["imageConfigs"].is_array());
    let assets: Value =
        serde_json::from_slice(&std::fs::read(std::env::var("AENV_DRAGONBALL_M0_ASSETS")?)?)?;
    assert_eq!(doc["imageConfigs"], assets["image_configs"]);
    assert!(
        output
            .stdout
            .contains(&format!("{id} dragonball-m0-existing-guest")),
        "guest identity environment: {:?}",
        output.stdout.lines().last()
    );
    assert!(
        result.is_err(),
        "wrong envd token accepted by process service"
    );
    let Err(envd::http_client::apis::Error::ResponseError(response)) = http_result else {
        anyhow::bail!("wrong envd token was not rejected with an HTTP authentication response");
    };
    assert_eq!(response.status.as_u16(), 401);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Linux x86_64 KVM/ublk/assets and an isolated outer network namespace"]
async fn dragonball_probe_metadata_survives_deny_all() -> Result<()> {
    let (mut probe, _) = start(true).await?;
    let output = probe.executor()?.run_command("/bin/sh", &["-ec", "t=$(curl --fail --silent -X PUT -H X-metadata-token-ttl-seconds:60 http://169.254.169.254/latest/api/token); curl --fail --silent -H \"X-metadata-token: $t\" http://169.254.169.254/envID"]).await?;
    probe.stop().await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, "dragonball-m0-existing-guest");
    Ok(())
}

#[tokio::test]
#[ignore = "requires two Linux x86_64 KVM/ublk guests in isolated network namespaces"]
async fn dragonball_probe_metadata_isolated_from_another_namespace() -> Result<()> {
    let (mut first, first_id) = start(false).await?;
    let (mut second, second_id) = start(false).await?;
    let token = first.executor()?.run_command("/bin/sh", &["-ec", "curl --fail --silent -X PUT -H X-metadata-token-ttl-seconds:60 http://169.254.169.254/latest/api/token"]).await?;
    assert_eq!(token.exit_code, 0);
    let opts =
        agentenv::sandbox::ProcessOpts::new().with_envs(std::collections::HashMap::from([(
            "OTHER_NAMESPACE_TOKEN".into(),
            token.stdout,
        )]));
    let output = second.executor()?.run_command_with_opts("/bin/sh", &["-ec", "curl --silent -o /tmp/m0-foreign-metadata -w '%{http_code}' -H \"X-metadata-token: $OTHER_NAMESPACE_TOKEN\" http://169.254.169.254/; printf '\\n'; cat /tmp/m0-foreign-metadata; printf '\\n'; t=$(curl --fail --silent -X PUT -H X-metadata-token-ttl-seconds:60 http://169.254.169.254/latest/api/token); curl --fail --silent -H \"X-metadata-token: $t\" http://169.254.169.254/instanceID"], &opts).await?;
    first.stop().await?;
    second.stop().await?;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(
        output.stdout.starts_with("401\n"),
        "foreign namespace token accepted"
    );
    assert!(!output.stdout.contains(&first_id));
    assert!(output.stdout.ends_with(&second_id));
    Ok(())
}
