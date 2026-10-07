//! Guest operations shared by VMM adapters. Tools are provided by AgentENV.
use super::{Executor, ProcessOpts};
use anyhow::{ensure, Context, Result};
use std::path::Path;

pub(crate) async fn mount_volume(
    executor: &Executor,
    index: usize,
    drive: &crate::sandbox::ExtraDrive,
) -> Result<()> {
    async fn command(executor: &Executor, name: &str, args: &[&str]) -> Result<()> {
        let mut tools_args = vec![name];
        tools_args.extend_from_slice(args);
        let mut output = executor
            .run_command_with_opts(
                "/agentenv/bin/busybox",
                &tools_args,
                &ProcessOpts::default().with_cwd("/"),
            )
            .await?;
        if name == "mount" && output.exit_code != 0 {
            // A restored guest can retain a mount from its previous runtime.
            // Retry only after removing it, never accept a stale target as success.
            let target = args.last().context("mount target")?;
            let _ = executor
                .run_command_with_opts(
                    "/agentenv/bin/busybox",
                    &["umount", "-l", "--", target],
                    &ProcessOpts::default().with_cwd("/"),
                )
                .await;
            output = executor
                .run_command_with_opts(
                    "/agentenv/bin/busybox",
                    &tools_args,
                    &ProcessOpts::default().with_cwd("/"),
                )
                .await?;
        }
        ensure!(
            output.exit_code == 0,
            "guest {name} failed: {}",
            output.stderr
        );
        Ok(())
    }
    ensure!(index < 24, "volume device index out of range");
    let device = format!("/dev/vd{}", char::from(b'c' + u8::try_from(index)?));
    let target = drive
        .mount_path()
        .to_str()
        .context("volume mount must be UTF-8")?;
    command(executor, "mkdir", &["-p", "--", target]).await?;
    let stage = format!("/run/agentenv-drive-{}", drive.drive_id());
    let mount = if drive.sub_path().is_some() {
        stage.as_str()
    } else {
        target
    };
    command(executor, "mkdir", &["-p", "--", mount]).await?;
    let options = if drive.read_only() { "ro,noload" } else { "rw" };
    command(executor, "mount", &["-o", options, "--", &device, mount]).await?;
    if let Some(sub_path) = drive.sub_path() {
        let source = Path::new(mount).join(sub_path);
        command(
            executor,
            "mount",
            &[
                "--bind",
                "--",
                source.to_str().context("volume subpath UTF-8")?,
                target,
            ],
        )
        .await?;
        if drive.read_only() {
            command(executor, "mount", &["-o", "remount,bind,ro", "--", target]).await?;
        }
        command(executor, "umount", &["-l", "--", mount]).await?;
    }
    Ok(())
}
