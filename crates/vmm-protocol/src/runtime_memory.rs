//! Transient, held-generation memory maps. Never embed these HVA-bearing types
//! in a durable snapshot record.
use crate::runtime::{CaptureToken, MemoryLayout, MemoryRegion};
use serde::{Deserialize, Serialize};

pub const MAX_MEMORY_RANGES_PER_CHUNK: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenMemoryRegion {
    pub guest_base: u64,
    pub host_base: u64,
    pub image_offset: u64,
    pub length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRange {
    pub image_offset: u64,
    pub length: u64,
}

// Intentionally not Serialize: only bounded chunks may cross the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenMemoryMap {
    pub token: CaptureToken,
    pub layout_id: String,
    pub page_size: u64,
    pub regions: Vec<FrozenMemoryRegion>,
    pub changed_ranges: Vec<MemoryRange>,
    /// Optional zero classification; changed_ranges still includes zero pages
    /// when the runner omits classification.
    pub zero_ranges: Vec<MemoryRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenMemoryMapChunk {
    pub token: CaptureToken,
    pub layout_id: String,
    pub page_size: u64,
    pub regions: Vec<FrozenMemoryRegion>,
    pub changed_ranges: Vec<MemoryRange>,
    pub zero_ranges: Vec<MemoryRange>,
    pub cursor: u64,
    pub total_changed_ranges: u64,
    pub total_zero_ranges: u64,
    pub next_cursor: Option<u64>,
}

pub struct MemoryMapPager {
    map: FrozenMemoryMap,
}

impl MemoryMapPager {
    pub fn new(map: FrozenMemoryMap) -> Result<Self, &'static str> {
        map.validate()?;
        Ok(Self { map })
    }

    pub fn page(&self, cursor: u64) -> Result<FrozenMemoryMapChunk, &'static str> {
        let start = usize::try_from(cursor).map_err(|_| "cursor overflow")?;
        let changed = self.map.changed_ranges.len();
        let total = changed + self.map.zero_ranges.len();
        if start % MAX_MEMORY_RANGES_PER_CHUNK != 0
            || (start >= total && !(start == 0 && total == 0))
        {
            return Err("invalid map cursor");
        }
        let end = start.saturating_add(MAX_MEMORY_RANGES_PER_CHUNK).min(total);
        Ok(FrozenMemoryMapChunk {
            token: self.map.token.clone(),
            layout_id: self.map.layout_id.clone(),
            page_size: self.map.page_size,
            regions: self.map.regions.clone(),
            changed_ranges: self.map.changed_ranges[start.min(changed)..end.min(changed)].to_vec(),
            zero_ranges: self.map.zero_ranges
                [start.saturating_sub(changed)..end.saturating_sub(changed)]
                .to_vec(),
            cursor,
            total_changed_ranges: self.map.changed_ranges.len() as u64,
            total_zero_ranges: self.map.zero_ranges.len() as u64,
            next_cursor: (end < total).then_some(end as u64),
        })
    }
}

fn checked_layout(
    page_size: u64,
    regions: &[FrozenMemoryRegion],
) -> Result<MemoryLayout, &'static str> {
    let mut hosts = Vec::new();
    let mut image_size = 0u64;
    for region in regions {
        let end = region
            .host_base
            .checked_add(region.length)
            .ok_or("HVA overflow")?;
        if region.host_base == 0 || region.host_base & 4095 != 0 || end > i64::MAX as u64 {
            return Err("invalid host mapping");
        }
        hosts.push((region.host_base, end));
        image_size = image_size
            .checked_add(region.length)
            .ok_or("image overflow")?;
    }
    hosts.sort_unstable();
    if hosts.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("overlapping host mappings");
    }
    let layout = MemoryLayout {
        page_size,
        image_size,
        regions: regions
            .iter()
            .map(|region| MemoryRegion {
                guest_base: region.guest_base,
                image_offset: region.image_offset,
                length: region.length,
            })
            .collect(),
    };
    layout.validate()?;
    Ok(layout)
}

fn valid_identity(token: &CaptureToken, layout_id: &str) -> Result<(), &'static str> {
    if token.generation == 0
        || token.runner_instance_id.is_empty()
        || token.runner_instance_id.len() > 256
        || layout_id.is_empty()
        || layout_id.len() > 256
    {
        return Err("invalid memory map identity");
    }
    Ok(())
}

fn checked_ranges(
    ranges: &[MemoryRange],
    mut previous_end: u64,
    image_size: u64,
) -> Result<(), &'static str> {
    for range in ranges {
        let end = range
            .image_offset
            .checked_add(range.length)
            .ok_or("range overflow")?;
        if range.length == 0
            || range.length & 4095 != 0
            || range.image_offset & 4095 != 0
            || range.image_offset < previous_end
            || end > image_size
        {
            return Err("unaligned, overlapping or out-of-bounds memory range");
        }
        previous_end = end;
    }
    Ok(())
}

fn checked_zeros<'a>(
    changed: impl Iterator<Item = &'a MemoryRange>,
    zeros: impl Iterator<Item = &'a MemoryRange>,
) -> Result<(), &'static str> {
    let mut changed = changed.peekable();
    for zero in zeros {
        let mut position = zero.image_offset;
        let end = position + zero.length; // Range bounds were checked first.
        while position < end {
            while changed
                .peek()
                .is_some_and(|range| range.image_offset + range.length <= position)
            {
                changed.next();
            }
            let range = changed.peek().ok_or("zero range is not dirty")?;
            if range.image_offset > position {
                return Err("zero range is not dirty");
            }
            position = end.min(range.image_offset + range.length);
        }
    }
    Ok(())
}

impl FrozenMemoryMap {
    pub fn validate(&self) -> Result<MemoryLayout, &'static str> {
        valid_identity(&self.token, &self.layout_id)?;
        let layout = checked_layout(self.page_size, &self.regions)?;
        checked_ranges(&self.changed_ranges, 0, layout.image_size)?;
        checked_ranges(&self.zero_ranges, 0, layout.image_size)?;
        checked_zeros(self.changed_ranges.iter(), self.zero_ranges.iter())?;
        Ok(layout)
    }
}

/// Sequential assembly bound to the already confirmed capture layout/token.
/// An invalid page leaves the cursor and all accepted data unchanged.
pub struct MemoryMapAssembler {
    token: CaptureToken,
    layout_id: String,
    layout: MemoryLayout,
    regions: Option<Vec<FrozenMemoryRegion>>,
    totals: Option<(u64, u64)>,
    changed: Vec<MemoryRange>,
    zeros: Vec<MemoryRange>,
    cursor: u64,
    complete: bool,
}

impl MemoryMapAssembler {
    pub fn new(
        token: CaptureToken,
        layout_id: String,
        layout: MemoryLayout,
    ) -> Result<Self, &'static str> {
        valid_identity(&token, &layout_id)?;
        layout.validate()?;
        Ok(Self {
            token,
            layout_id,
            layout,
            regions: None,
            totals: None,
            changed: Vec::new(),
            zeros: Vec::new(),
            cursor: 0,
            complete: false,
        })
    }

    pub fn push(
        &mut self,
        chunk: FrozenMemoryMapChunk,
    ) -> Result<Option<FrozenMemoryMap>, &'static str> {
        if self.complete
            || chunk.token != self.token
            || chunk.layout_id != self.layout_id
            || chunk.cursor != self.cursor
            || checked_layout(chunk.page_size, &chunk.regions)? != self.layout
            || self
                .regions
                .as_ref()
                .is_some_and(|regions| *regions != chunk.regions)
            || self.totals.is_some_and(|totals| {
                totals != (chunk.total_changed_ranges, chunk.total_zero_ranges)
            })
            || chunk.total_changed_ranges > self.layout.image_size / self.layout.page_size
            || chunk.total_zero_ranges > self.layout.image_size / self.layout.page_size
        {
            return Err("memory chunk identity, layout or cursor mismatch");
        }
        let total = chunk
            .total_changed_ranges
            .checked_add(chunk.total_zero_ranges)
            .ok_or("range count overflow")?;
        if chunk.cursor > total || (chunk.cursor == total && total != 0) {
            return Err("cursor past last range");
        }
        let end = chunk
            .cursor
            .saturating_add(MAX_MEMORY_RANGES_PER_CHUNK as u64)
            .min(total);
        let expected_changed =
            end.min(chunk.total_changed_ranges) - chunk.cursor.min(chunk.total_changed_ranges);
        let expected_zeros = end - chunk.cursor - expected_changed;
        if chunk.changed_ranges.len() as u64 != expected_changed
            || chunk.zero_ranges.len() as u64 != expected_zeros
            || chunk.next_cursor != (end < total).then_some(end)
        {
            return Err("missing, oversized or discontinuous memory page");
        }
        checked_ranges(
            &chunk.changed_ranges,
            self.changed
                .last()
                .map_or(0, |range| range.image_offset + range.length),
            self.layout.image_size,
        )?;
        checked_ranges(
            &chunk.zero_ranges,
            self.zeros
                .last()
                .map_or(0, |range| range.image_offset + range.length),
            self.layout.image_size,
        )?;
        if end == total {
            checked_zeros(
                self.changed.iter().chain(&chunk.changed_ranges),
                self.zeros.iter().chain(&chunk.zero_ranges),
            )?;
        }
        self.regions = Some(chunk.regions);
        self.totals = Some((chunk.total_changed_ranges, chunk.total_zero_ranges));
        self.changed.extend(chunk.changed_ranges);
        self.zeros.extend(chunk.zero_ranges);
        self.cursor = end;
        self.complete = end == total;
        Ok(self.complete.then(|| FrozenMemoryMap {
            token: self.token.clone(),
            layout_id: self.layout_id.clone(),
            page_size: self.layout.page_size,
            regions: self.regions.take().unwrap(),
            changed_ranges: std::mem::take(&mut self.changed),
            zero_ranges: std::mem::take(&mut self.zeros),
        }))
    }
}
