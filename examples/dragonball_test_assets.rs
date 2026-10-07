//! Resolve/materialize guest assets before entering the offline test namespace.
use std::{fs, path::PathBuf};

use agentenv::{cfg::ConfigManager, image::ImageResolver, types::ImageConfigs};
use anyhow::{Context, Result};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    anyhow::ensure!(
        args.len() == 2,
        "usage: dragonball_test_assets <runner> <output.json>"
    );
    let config = ConfigManager::init_global()?.config();
    let source = std::env::var("AENV_DRAGONBALL_M0_IMAGE")
        .unwrap_or_else(|_| "ghcr.io/linuxserver/baseimage-ubuntu:noble".into());
    let resolved = ImageResolver::new(config).resolve(&source).await?;
    let image_path = fs::canonicalize(resolved.overlaybd_config_path)?;
    // The fixture must be usable without network access after namespace entry.
    // Reject unresolved lazy lower layers before entering the offline namespace.
    let image = overlaybd::config::load_image_config(&image_path)?;
    for lower in &image.lowers {
        anyhow::ensure!(
            PathBuf::from(&lower.file).is_file(),
            "M0 isolated guest tests require locally materialized lower {}",
            lower.file
        );
    }
    let mut configs = ImageConfigs::new();
    configs.add(
        None::<String>,
        "/",
        resolved
            .raw_config
            .context("qualification image must retain its source config")?,
    );
    let output = PathBuf::from(&args[1]);
    fs::create_dir_all(output.parent().context("asset output parent")?)?;
    let assets = json!({
        "runner_path": fs::canonicalize(&args[0])?,
        "kernel_path": fs::canonicalize(config.resolved_kernel_image_path())?,
        "tools_path": fs::canonicalize(config.resolved_tools_drive_path()?)?,
        "image_path": image_path,
        "image_configs": configs.to_value(),
    });
    fs::write(output, serde_json::to_vec_pretty(&assets)?)?;
    Ok(())
}
