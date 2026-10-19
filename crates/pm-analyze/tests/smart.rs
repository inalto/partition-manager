use pm_analyze::{smart_summary, Health};
use pm_platform::{SmartAttr, SmartData};

fn attr(id: u8, raw: u64) -> SmartAttr {
    SmartAttr { id, name: format!("attr{id}"), value: 100, worst: 100, threshold: 10, raw }
}

fn row<'a>(rows: &'a [(String, String, Health)], name: &str) -> &'a (String, String, Health) {
    rows.iter().find(|r| r.0 == name).unwrap_or_else(|| panic!("no row {name}: {rows:?}"))
}

#[test]
fn smart_summary_flags_reallocated_sectors() {
    let d = SmartData {
        healthy: Some(true),
        temperature_c: Some(41),
        power_on_hours: Some(1234),
        nvme_percent_used: None,
        attrs: vec![attr(5, 8), attr(197, 0), attr(198, 500), attr(199, 2), attr(1, 77)],
    };
    let rows = smart_summary(&d);
    assert_eq!(row(&rows, "Health").2, Health::Ok);
    assert_eq!(row(&rows, "Temperature").1, "41 °C");
    assert_eq!(row(&rows, "Temperature").2, Health::Ok);
    assert_eq!(row(&rows, "Power-on hours").1, "1234 h");
    assert_eq!(row(&rows, "attr5").2, Health::Warn);
    assert_eq!(row(&rows, "attr197").2, Health::Ok);
    assert_eq!(row(&rows, "attr198").2, Health::Bad);
    assert_eq!(row(&rows, "attr199").2, Health::Warn);
    assert_eq!(row(&rows, "attr1").2, Health::Ok);
}

#[test]
fn smart_summary_thresholds() {
    let mut d = SmartData { healthy: Some(false), temperature_c: Some(55), ..Default::default() };
    let rows = smart_summary(&d);
    assert_eq!(row(&rows, "Health").2, Health::Bad);
    assert_eq!(row(&rows, "Temperature").2, Health::Warn);
    d.temperature_c = Some(70);
    assert_eq!(row(&smart_summary(&d), "Temperature").2, Health::Bad);
    d.temperature_c = Some(54);
    assert_eq!(row(&smart_summary(&d), "Temperature").2, Health::Ok);

    d.nvme_percent_used = Some(79);
    assert_eq!(row(&smart_summary(&d), "Percentage used").2, Health::Ok);
    d.nvme_percent_used = Some(80);
    assert_eq!(row(&smart_summary(&d), "Percentage used").2, Health::Warn);
    d.nvme_percent_used = Some(100);
    assert_eq!(row(&smart_summary(&d), "Percentage used").2, Health::Bad);

    d.attrs = vec![attr(187, 101), attr(5, 100)];
    let rows = smart_summary(&d);
    assert_eq!(row(&rows, "attr187").2, Health::Bad);
    assert_eq!(row(&rows, "attr5").2, Health::Warn);

    // unknown health is not reported as OK
    let rows = smart_summary(&SmartData::default());
    assert_eq!(row(&rows, "Health").2, Health::Warn);
    assert!(rows.iter().all(|r| r.0 != "Temperature"));
}
