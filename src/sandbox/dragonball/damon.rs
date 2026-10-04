use agentenv_vmm_protocol::runtime::MemoryLayout;
use anyhow::{ensure, Result};

pub(super) fn damon_monitor_bounds(layout: &MemoryLayout) -> Result<(u64, u64)> {
    layout.validate().map_err(anyhow::Error::msg)?;
    // Zero means "auto-select" to DAMON_RECLAIM. Skip page zero rather than
    // accidentally selecting only the kernel's largest contiguous RAM region.
    let first = &layout.regions[0];
    let start = first.guest_base.max(layout.page_size);
    let last = layout.regions.last().unwrap();
    let end = last.guest_base + last.length; // checked by validate()
    ensure!(
        start < first.guest_base + first.length && start < end,
        "no monitorable RAM"
    );
    Ok((start, end))
}

pub(super) fn boot_args(layout: &MemoryLayout) -> Result<String> {
    let (start, end) = damon_monitor_bounds(layout)?;
    // Preserve the existing FC reclaim policy. This is one physical-address
    // interval, not multiple RAM intervals: Linux 6.1 damon_get_page() rejects
    // offline/non-LRU PFNs, including the MMIO hole in larger guests.
    Ok(format!("damon_reclaim.enabled=Y damon_reclaim.min_age=100000 damon_reclaim.quota_ms=20 damon_reclaim.quota_sz=1073741824 damon_reclaim.quota_reset_interval_ms=500 damon_reclaim.wmarks_high=990 damon_reclaim.wmarks_mid=990 damon_reclaim.wmarks_low=200 damon_reclaim.skip_anon=Y damon_reclaim.wmarks_interval=1000000 damon_reclaim.monitor_region_start={start} damon_reclaim.monitor_region_end={end}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentenv_vmm_protocol::runtime::MemoryRegion;

    #[test]
    fn checked_monitor_bounds_cover_high_ram_without_treating_the_hole_as_packed_bytes() {
        for (mib, end) in [(1024, 1073741824), (3072, 3221225472), (8192, 9663676416)] {
            assert_eq!(
                damon_monitor_bounds(&MemoryLayout::for_runtime_memory(mib)).unwrap(),
                (4096, end)
            );
        }
        let high_only = MemoryLayout {
            page_size: 4096,
            image_size: 8192,
            regions: vec![MemoryRegion {
                guest_base: 0x1_0000_0000,
                image_offset: 0,
                length: 8192,
            }],
        };
        assert_eq!(
            damon_monitor_bounds(&high_only).unwrap(),
            (0x1_0000_0000, 0x1_0000_2000)
        );
        let mut invalid = high_only.clone();
        invalid.regions[0].guest_base = u64::MAX - 4095;
        assert!(damon_monitor_bounds(&invalid).is_err());
        invalid = high_only;
        invalid.image_size = 4096;
        assert!(damon_monitor_bounds(&invalid).is_err());
        assert!(damon_monitor_bounds(&MemoryLayout::for_runtime_memory(0)).is_err());
    }
}
