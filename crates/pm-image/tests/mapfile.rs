//! GNU ddrescue-compatible mapfile parsing, printing and bookkeeping.

use pm_core::Error;
use pm_image::rescue::{BlockStatus, MapFile, MapStats};
use BlockStatus::*;

const GNU_SAMPLE: &str = "\
# Mapfile. Created by GNU ddrescue version 1.27
# Command line: ddrescue -d /dev/sdb out.img out.map
# Start time:   2024-01-01 10:00:00
# Current time: 2024-01-01 10:05:00
# Finished
# current_pos  current_status  current_pass
0x00400000     +               1
#      pos        size  status
0x00000000  0x00100000  +
0x00100000  0x00000200  -
0x00100200  0x002FFE00  +
";

/// Blocks are non-empty, contiguous, cover [0, len) and no two neighbours share a status.
fn assert_invariant(m: &MapFile, len: u64) {
    let mut pos = 0;
    for (i, &(p, s, st)) in m.blocks.iter().enumerate() {
        assert_eq!(p, pos, "block {i} not contiguous: {:?}", m.blocks);
        assert!(s > 0, "empty block {i}: {:?}", m.blocks);
        if i > 0 {
            assert_ne!(m.blocks[i - 1].2, st, "uncoalesced at {i}: {:?}", m.blocks);
        }
        pos += s;
    }
    assert_eq!(pos, len);
    assert_eq!(m.len(), len);
}

#[test]
fn mapfile_roundtrip_gnu_format() {
    let m = MapFile::parse(GNU_SAMPLE).unwrap();
    assert_eq!(m.current_pos, 0x400000);
    assert_eq!(m.current_status, '+');
    assert_eq!(m.pass, 1);
    assert_eq!(
        m.blocks,
        vec![
            (0, 0x100000, Finished),
            (0x100000, 0x200, Bad),
            (0x100200, 0x2FFE00, Finished)
        ]
    );
    let st = m.stats();
    assert_eq!(st.rescued, 0x3FFE00);
    assert_eq!(st.bad_bytes, 0x200);
    assert_eq!(st.bad_areas, 1);
    assert_invariant(&m, 0x400000);

    let text = m.to_string();
    assert!(text.starts_with("# Mapfile. Created by pm (Partition Manager)\n"), "{text}");
    assert!(text.contains("# current_pos  current_status  current_pass\n0x00400000     +               1\n"), "{text}");
    assert!(text.contains("#      pos        size  status\n"), "{text}");
    assert!(text.contains("0x00100000  0x00000200  -\n"), "{text}");
    assert!(text.contains("0x00100200  0x002ffe00  +\n"), "{text}");
    let again = MapFile::parse(&text).unwrap();
    assert_eq!(again, m);
}

#[test]
fn mapfile_parse_tolerates_crlf_trailing_spaces_and_missing_pass() {
    let s = "# Mapfile. Created by GNU ddrescue version 1.21\r\n\
             # extra comment  \r\n\
             0x00000200     F  \r\n\
             \r\n\
             #      pos        size  status\r\n\
             0x00000000  0x00000200  +   \r\n\
             0X00000200  0x00000a00  *\r\n\
             3072  1024  /\r\n\
             0x00001000  0x00001000  ?\r\n";
    let m = MapFile::parse(s).unwrap();
    assert_eq!(m.current_pos, 0x200);
    assert_eq!(m.current_status, '+', "F (filling) is read as finished");
    assert_eq!(m.pass, 1, "missing current_pass defaults to 1");
    assert_eq!(
        m.blocks,
        vec![(0, 0x200, Finished), (0x200, 0xA00, NonTrimmed), (0xC00, 0x400, NonScraped), (0x1000, 0x1000, NonTried)]
    );
    assert_eq!(MapFile::parse(&m.to_string()).unwrap(), m);
}

#[test]
fn mapfile_parse_rejects_garbage() {
    for bad in [
        "",
        "# only comments\n",
        "0x0 ? 1\n0x0 0x200 X\n",
        "0x0 Q 1\n",
        "0x0 ? 1\n0x0 0x200 +\n0x100 0x200 +\n", // overlap
        "0x0 ? 1\n0x0 zz +\n",
        "0x0 ? 1\n0x0 0x200\n",
        "0x0 ? 1\n0xFFFFFFFFFFFFFFFF 0x200 +\n", // overflow
    ] {
        assert!(matches!(MapFile::parse(bad), Err(Error::Parse(_))), "accepted {bad:?}");
    }
}

#[test]
fn mapfile_parse_fills_gaps_as_non_tried() {
    let m = MapFile::parse("0x0 ? 1\n0x400 0x200 +\n0x800 0x200 -\n").unwrap();
    assert_eq!(m.blocks, vec![(0, 0x400, NonTried), (0x400, 0x200, Finished), (0x600, 0x200, NonTried), (0x800, 0x200, Bad)]);
}

#[test]
fn mapfile_stats() {
    let mut m = MapFile::new(1 << 20);
    assert_eq!(m.blocks, vec![(0, 1 << 20, NonTried)]);
    assert_eq!(m.current_status, '?');
    assert_eq!(m.pass, 1);
    m.mark(0, 4096, Finished);
    m.mark(4096, 512, Bad);
    m.mark(8192, 1024, NonTrimmed);
    m.mark(16384, 2048, NonScraped);
    m.mark(32768, 512, Bad);
    m.mark(33280, 512, Bad); // adjacent: coalesces with the previous bad block
    m.mark(65536, 1024, Finished);
    assert_invariant(&m, 1 << 20);
    assert_eq!(
        m.stats(),
        MapStats {
            rescued: 4096 + 1024,
            non_tried: (1 << 20) - 4096 - 512 - 1024 - 2048 - 1024 - 1024,
            non_trimmed: 1024,
            non_scraped: 2048,
            bad_bytes: 1024 + 512,
            bad_areas: 2,
        }
    );
    assert_eq!(m.ranges(Bad), vec![(4096, 512), (32768, 1024)]);
    // marking across several blocks replaces them all
    m.mark(0, 40000, Finished);
    assert_invariant(&m, 1 << 20);
    assert_eq!(m.ranges(Finished), vec![(0, 40000), (65536, 1024)]);
    assert_eq!(m.stats().bad_areas, 0);
    // out-of-range marks are clipped
    m.mark((1 << 20) - 512, 4096, Bad);
    m.mark(1 << 20, 4096, Bad);
    assert_invariant(&m, 1 << 20);
    assert_eq!(m.ranges(Bad), vec![((1 << 20) - 512, 512)]);
    // everything finished collapses to one block
    m.mark(0, 1 << 20, Finished);
    assert_eq!(m.blocks, vec![(0, 1 << 20, Finished)]);
}

#[test]
fn mapfile_merge_coalesce() {
    let mut m = MapFile::new(0);
    m.blocks = vec![(0, 512, Finished), (512, 0, Bad), (512, 512, Finished), (1024, 512, Bad), (1536, 512, Bad)];
    m.merge_coalesce();
    assert_eq!(m.blocks, vec![(0, 1024, Finished), (1024, 1024, Bad)]);
}

#[test]
fn mapfile_save_load() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.map");
    let mut m = MapFile::new(10_000);
    m.mark(512, 512, Bad);
    m.current_pos = 1024;
    m.current_status = '*';
    m.save(&p).unwrap();
    assert_eq!(MapFile::load(&p).unwrap(), m);
    // saving again replaces the file and leaves no temporary behind
    m.mark(0, 512, Finished);
    m.save(&p).unwrap();
    assert_eq!(MapFile::load(&p).unwrap(), m);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["x.map".to_string()]);
    assert!(MapFile::load(&dir.path().join("missing.map")).is_err());
}
