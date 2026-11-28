//! GPU timestamps: a query pool per frame slot, written by
//! [`crate::Op::Timestamp`] as replay meets it, read once the slot's frame
//! retired, never waited for.
//!
//! ```text
//! submit: reset the slot's pool ──replay──► write stamp k after all earlier work, ids[k] = id
//! begin_frame: newest retired slot with stamps ──► times: (id, ns)
//! ```

use ash::vk;

use super::Vk;
use super::convert::err;
use crate::GpuError;

/// Most stamps one frame records, the rest are dropped.
pub(super) const MAX_STAMPS: u32 = 64;

/// A slot's pool and the ids of the stamps its last submit wrote, in
/// query order. A null pool when the device has no timestamps.
#[derive(Default)]
pub(super) struct Stamps {
    pub(super) pool: vk::QueryPool,
    pub(super) ids: Vec<u8>,
}

impl Stamps {
    /// A pool of `MAX_STAMPS` timestamps.
    pub(super) fn new(d: &ash::Device) -> Result<Stamps, GpuError> {
        let info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(MAX_STAMPS);
        // SAFETY: a plain create on a live device.
        let pool = unsafe { d.create_query_pool(&info, None) }.map_err(err)?;
        Ok(Stamps {
            pool,
            ids: Vec::with_capacity(MAX_STAMPS as usize),
        })
    }

    /// Reset the pool at the start of a frame's commands.
    pub(super) fn reset(&mut self, d: &ash::Device, cb: vk::CommandBuffer) {
        self.ids.clear();
        if self.pool != vk::QueryPool::null() {
            // SAFETY: cb is recording outside any pass, the pool is ours
            // and its last frame retired.
            unsafe { d.cmd_reset_query_pool(cb, self.pool, 0, MAX_STAMPS) };
        }
    }

    /// Write stamp `id` after everything recorded before it.
    pub(super) fn write(&mut self, d: &ash::Device, cb: vk::CommandBuffer, id: u8) {
        let k = self.ids.len() as u32;
        if self.pool == vk::QueryPool::null() || k >= MAX_STAMPS {
            return;
        }
        // SAFETY: the pool was reset in this command buffer, query k unused.
        unsafe { d.cmd_write_timestamp2(cb, vk::PipelineStageFlags2::ALL_COMMANDS, self.pool, k) };
        self.ids.push(id);
    }
}

impl Vk {
    pub(crate) fn timestamps(&self) -> &[(u8, u64)] {
        &self.times
    }

    /// Read the stamps of the newest slot whose frame retired into
    /// `times`, in nanoseconds. Allocates nothing.
    pub(super) fn read_stamps(&mut self) -> Result<(), GpuError> {
        let done = self.completed()?.0;
        let newest = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s.state {
                super::SlotState::InFlight(p) if p <= done && !s.stamps.ids.is_empty() => {
                    Some((p, i))
                }
                _ => None,
            })
            .max();
        let Some((_, i)) = newest else { return Ok(()) };
        let s = &mut self.slots[i].stamps;
        let n = s.ids.len();
        let raw = &mut self.stamp_raw[..n];
        // SAFETY: queries 0..n were written by a submit that retired.
        unsafe {
            self.device
                .get_query_pool_results(s.pool, 0, raw, vk::QueryResultFlags::TYPE_64)
        }
        .map_err(err)?;
        self.times.clear();
        let (period, mask) = (self.stamp_period, self.stamp_mask);
        let ns = raw.iter().map(|&t| ((t & mask) as f64 * period) as u64);
        self.times.extend(s.ids.iter().copied().zip(ns));
        s.ids.clear();
        Ok(())
    }
}
