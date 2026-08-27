//! 阶段 4.5 图表渲染测试
//!
//! 用 mock series 渲染 PNG,断言文件存在 + 尺寸正确 + 字节数 > 0。

use lnms_invoice::chart::{render_95th_png, SeriesPoint};
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn test_render_with_series() {
    let dir = tempdir().unwrap();
    let out: PathBuf = dir.path().join("chart.png");

    // 模拟 5min 一月 8928 点太慢,用 100 点;验证能渲染
    let series: Vec<SeriesPoint> = (0..100)
        .map(|i| {
            let ts = 1_700_000_000 + (i as i64) * 300;
            let v = 100.0 + ((i as f64) * 0.5).sin() * 50.0;
            (ts, v)
        })
        .collect();

    render_95th_png(&series, &out, 2069, 713, Some(150.0)).expect("render");
    assert!(out.exists());
    let meta = std::fs::metadata(&out).unwrap();
    assert!(meta.len() > 1000, "png 应 > 1KB,实际 {}", meta.len());

    // 验证 PNG header + 尺寸
    let dim = image::image_dimensions(&out).expect("png dims");
    assert_eq!(dim, (2069, 713));
}

#[test]
fn test_render_empty_series_with_p95() {
    let dir = tempdir().unwrap();
    let out = dir.path().join("empty.png");
    render_95th_png(&[], &out, 800, 400, Some(100.0)).expect("render");
    assert!(out.exists());
    let dim = image::image_dimensions(&out).expect("png dims");
    assert_eq!(dim, (800, 400));
}

#[test]
fn test_render_no_p95_line() {
    let dir = tempdir().unwrap();
    let out = dir.path().join("no_p95.png");
    let series: Vec<SeriesPoint> = (0..50).map(|i| (i as i64, (i as f64) * 2.0)).collect();
    render_95th_png(&series, &out, 600, 300, None).expect("render");
    assert!(out.exists());
}

#[test]
fn test_zero_size_rejected() {
    let dir = tempdir().unwrap();
    let out = dir.path().join("bad.png");
    let res = render_95th_png(&[], &out, 0, 100, None);
    assert!(res.is_err());
    let res = render_95th_png(&[], &out, 100, 0, None);
    assert!(res.is_err());
}
