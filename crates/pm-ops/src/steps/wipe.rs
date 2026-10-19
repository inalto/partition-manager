//! Overwrite a partition or a whole disk.

use super::table_ops::zero_table_areas;
use super::{with_session, Ctx, CHUNK};
use crate::operation::WipeMethod;
use pm_core::{BlockDevice, CancelToken, ProgressSink, ProgressTracker, Result};
use uuid::Uuid;

/// xorshift64 pseudo-random stream (fast; not cryptographic, which wiping does not need).
struct XorShift(u64);

impl XorShift {
    fn seeded() -> Self {
        let b = Uuid::new_v4().into_bytes();
        let seed = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        XorShift(seed | 1)
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
    }
}

#[derive(Clone, Copy)]
enum Pass {
    Fill(u8),
    Random,
}

fn passes(m: WipeMethod) -> &'static [Pass] {
    match m {
        WipeMethod::Zero => &[Pass::Fill(0)],
        WipeMethod::Random => &[Pass::Random],
        WipeMethod::Dod3 => &[Pass::Fill(0x00), Pass::Fill(0xFF), Pass::Random],
    }
}

/// Write the passes of `method` over `[start, start + len)` in 4 MiB chunks.
fn wipe_range(
    dev: &mut dyn BlockDevice,
    start: u64,
    len: u64,
    method: WipeMethod,
    sink: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<()> {
    let passes = passes(method);
    let mut tracker = ProgressTracker::new(sink, "Wiping", len.saturating_mul(passes.len() as u64));
    let mut buf = vec![0u8; CHUNK.min(len) as usize];
    for (i, pass) in passes.iter().enumerate() {
        tracker.set_detail(&format!("pass {} of {}", i + 1, passes.len()));
        let mut rng = XorShift::seeded();
        if let Pass::Fill(b) = pass {
            buf.fill(*b);
        }
        let mut off = 0;
        while off < len {
            cancel.check()?;
            let n = (len - off).min(CHUNK) as usize;
            if let Pass::Random = pass {
                rng.fill(&mut buf[..n]);
            }
            dev.write_at(start + off, &buf[..n])?;
            off += n as u64;
            tracker.add(n as u64);
        }
        dev.flush()?;
    }
    tracker.finish();
    Ok(())
}

pub(crate) fn wipe_partition(ctx: &mut Ctx, disk: u32, number: u32, method: WipeMethod) -> Result<()> {
    let p = ctx.live_partition(disk, number)?;
    let (start, len) = (p.start, p.len);
    let sink = ctx.step("wipe");
    let cancel = ctx.cancel;
    with_session(ctx.platform, disk, |s| wipe_range(s.device(), start, len, method, &sink, cancel))?;
    ctx.rescan(disk)
}

/// Wipe every byte of the disk, then zero sector 0 and both GPT areas so that no partition
/// table is left (a random last pass could otherwise look like one). `commit_table` cannot
/// express "no table" (`write_table` refuses kind `None`), so the layout is re-read with
/// `rescan` instead.
pub(crate) fn wipe_disk(ctx: &mut Ctx, disk: u32, method: WipeMethod) -> Result<()> {
    let sink = ctx.step("wipe");
    let cancel = ctx.cancel;
    with_session(ctx.platform, disk, |s| {
        let dev = s.device();
        let len = dev.len();
        wipe_range(dev, 0, len, method, &sink, cancel)?;
        zero_table_areas(dev, false, &pm_parttable::Table::empty(pm_core::TableKind::None, len, 512))?;
        dev.flush()
    })?;
    ctx.rescan(disk)
}
