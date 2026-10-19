use pm_fs::UsedMap;

#[test]
fn set_marks_every_touched_unit() {
    let mut m = UsedMap::new(100, 10);
    assert_eq!(m.bits.len(), 2);
    m.set(15, 10); // touches units 1 and 2
    assert!(!m.is_used(9));
    assert!(m.is_used(10) && m.is_used(19) && m.is_used(20) && m.is_used(29));
    assert!(!m.is_used(30));
    assert_eq!(m.bits[0], 0b0000_0110, "little-endian bit order: unit i is bit i%8 of byte i/8");
    m.set(80, 1000); // clipped to len
    assert!(m.is_used(99));
    assert!(!m.is_used(100), "outside the filesystem is never used");
    assert_eq!(m.bits[1], 0b0000_0011);
    m.set(50, 0); // empty range marks nothing
    assert!(!m.is_used(50));
    m.set(5000, 10); // beyond len is ignored
}

#[test]
fn ranges_are_coalesced_and_clipped() {
    let mut m = UsedMap::new(95, 10); // last unit is partial: bytes 90..95
    m.set(0, 10);
    m.set(10, 20); // adjacent: coalesces with the first
    m.set(50, 10);
    m.set(90, 5);
    let r: Vec<_> = m.ranges().collect();
    assert_eq!(r, vec![(0, 30), (50, 10), (90, 5)]);
    assert_eq!(m.used_bytes(), 45);
}

#[test]
fn ranges_handle_byte_boundaries_and_long_runs() {
    let mut m = UsedMap::new(4096 * 100, 4096);
    m.set(4096 * 7, 4096 * 2); // units 7, 8: spans a byte boundary
    m.set(4096 * 16, 4096 * 24); // units 16..40: whole bytes of 0xFF
    m.set(4096 * 99, 4096);
    let r: Vec<_> = m.ranges().collect();
    assert_eq!(r, vec![(4096 * 7, 4096 * 2), (4096 * 16, 4096 * 24), (4096 * 99, 4096)]);
}

#[test]
fn all_used_and_empty() {
    let m = UsedMap::all_used(10_000);
    assert_eq!(m.unit, 4096);
    assert_eq!(m.len, 10_000);
    assert_eq!(m.ranges().collect::<Vec<_>>(), vec![(0, 10_000)]);
    assert_eq!(m.used_bytes(), 10_000);
    assert!(m.is_used(9_999) && !m.is_used(10_000));
    let e = UsedMap::new(10_000, 512);
    assert_eq!(e.ranges().count(), 0);
    assert_eq!(e.used_bytes(), 0);
    assert_eq!(UsedMap::new(0, 512).ranges().count(), 0);
    let exact = UsedMap::all_used(8 * 4096);
    assert_eq!(exact.ranges().collect::<Vec<_>>(), vec![(0, 8 * 4096)]);
}

#[test]
fn bitmap_origin_units_align() {
    let mut m = UsedMap::with_origin(4096 * 10, 4096, 512);
    assert_eq!(m.origin, 512);
    // [512, 4608) is exactly one unit
    m.set(512, 4096);
    assert_eq!(m.ranges().collect::<Vec<_>>(), vec![(512, 4096)]);
    assert_eq!(m.used_bytes(), 4096);
    assert!(!m.is_used(511));
    assert!(m.is_used(512) && m.is_used(4607));
    assert!(!m.is_used(4608));
    // the short head unit covers [0, 512)
    m.set(0, 10);
    assert!(m.is_used(0) && m.is_used(511));
    assert_eq!(m.ranges().collect::<Vec<_>>(), vec![(0, 4608)]);
    // the tail is clipped to len
    m.set(4096 * 10 - 1, 1);
    let r: Vec<_> = m.ranges().collect();
    assert_eq!(r.last().copied(), Some((512 + 4096 * 9, 4096 * 10 - (512 + 4096 * 9))));
    assert!(m.ranges().all(|(s, l)| s + l <= 4096 * 10));
    // plain maps keep origin 0; `with_origin` reduces the origin modulo the unit
    assert_eq!(UsedMap::new(100, 10).origin, 0);
    assert_eq!(UsedMap::with_origin(100, 10, 13).origin, 3);
    let mut z = UsedMap::with_origin(100, 10, 0);
    z.set(10, 10);
    assert_eq!(z.ranges().collect::<Vec<_>>(), vec![(10, 10)]);
}
