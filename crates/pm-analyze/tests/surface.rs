use pm_analyze::surface_test;
use pm_core::{BlockDevice, CancelToken, Error, MemDevice, NullProgress, Result};
use pm_image::rescue::FaultyDevice;
use std::collections::HashMap;
use std::time::Duration;

/// Wall-clock assertions are unreliable on shared CI runners (a noisy neighbour can make any
/// block look slow), so tests that depend on timing are skipped when `CI` is set.
fn on_ci() -> bool {
    std::env::var_os("CI").is_some_and(|v| !v.is_empty() && v != "false" && v != "0")
}

#[test]
fn surface_reports_bad_block_from_faulty_device() {
    // 10 blocks of 4 KiB plus a 100-byte tail block; one bad sector inside block 3
    let len = 10 * 4096 + 100;
    let mut dev = FaultyDevice::new(MemDevice::new(len), vec![3 * 4096 + 512], HashMap::new());
    let rep = surface_test(&mut dev, 4096, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(rep.block, 4096);
    assert_eq!(rep.blocks, 11);
    assert_eq!(rep.bad, vec![3]);
    assert_eq!(rep.ms_per_block.len(), 11);
    // timing-dependent: no block may be flagged slow
    if !on_ci() {
        assert!(rep.slow.is_empty(), "{:?}", rep.slow);
    }
    assert!(rep.elapsed_secs >= 0.0);
}

/// Sleeps on one block to make it an outlier.
struct SlowDevice {
    inner: MemDevice,
    slow_offset: u64,
}

impl BlockDevice for SlowDevice {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset == self.slow_offset {
            std::thread::sleep(Duration::from_millis(30));
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

#[test]
fn surface_flags_slow_blocks_over_ten_times_median() {
    if on_ci() {
        eprintln!("skipping: wall-clock sensitive test under CI");
        return;
    }
    let mut dev = SlowDevice { inner: MemDevice::new(8 * 4096), slow_offset: 5 * 4096 };
    let rep = surface_test(&mut dev, 4096, &NullProgress, &CancelToken::new()).unwrap();
    assert!(rep.bad.is_empty());
    assert_eq!(rep.slow, vec![5]);
    assert!(rep.ms_per_block[5] >= 30);
}

#[test]
fn surface_cancel_and_invalid_block() {
    let mut dev = MemDevice::new(1 << 20);
    let c = CancelToken::new();
    c.cancel();
    assert!(matches!(surface_test(&mut dev, 4096, &NullProgress, &c), Err(Error::Cancelled)));
    assert!(matches!(
        surface_test(&mut dev, 0, &NullProgress, &CancelToken::new()),
        Err(Error::Invalid(_))
    ));
    // empty device: zero blocks
    let mut empty = MemDevice::new(0);
    let rep = surface_test(&mut empty, 4096, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(rep.blocks, 0);
}
