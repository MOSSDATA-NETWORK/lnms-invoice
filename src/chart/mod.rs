//! 95th Mbps 曲线图渲染(阶段 4.5)
//!
//! 用 plotters bitmap backend 渲染与模板 picture 同尺寸的 PNG(默认 2069×713,
//! 与 LNMS 95th 图表同尺寸,阶段 5 实装填充时直接字节级替换)。
//!
//! 标签纯英文,避免 CJK 字体依赖;x 轴按时间索引(不渲染日期文字,只画 tick 标号),
//! y 轴 Mbps。
//!
//! 用途:阶段 5 把这个 PNG 直接覆盖模板里的 `xl/media/image1.png`。

use crate::error::{Error, Result};
use plotters::prelude::*;
use std::path::Path;

/// 时间序列点(unix timestamp 秒, Mbps)
pub type SeriesPoint = (i64, f64);

/// 渲染曲线图到 PNG。
///
/// - `series`:升序时间序列,空序列会渲染空白图(只有网格)
/// - `output_path`:输出 PNG 路径
/// - `width` / `height`:像素尺寸(默认与模板 picture 一致)
/// - `p95`:95 百分位值(画一条水平参考线;`None` 不画)
pub fn render_95th_png(
    series: &[SeriesPoint],
    output_path: &Path,
    width: u32,
    height: u32,
    p95: Option<f64>,
) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(Error::Template(format!(
            "chart size must be > 0: {width}x{height}"
        )));
    }

    let root = BitMapBackend::new(output_path, (width, height)).into_drawing_area();
    root.fill(&WHITE)
        .map_err(|e| Error::Template(format!("chart fill bg: {e}")))?;

    let max_y = series
        .iter()
        .map(|(_, v)| *v)
        .fold(0f64, f64::max)
        .max(p95.unwrap_or(0.0))
        * 1.1;
    let max_y = if max_y <= 0.0 { 1.0 } else { max_y };

    let n = series.len();
    let x_end = if n == 0 { 1 } else { n };

    let mut chart = ChartBuilder::on(&root)
        .margin(10)
        .caption("95th Percentile Mbps", ("sans-serif", 20))
        .x_label_area_size(40)
        .y_label_area_size(80)
        .build_cartesian_2d(0..x_end, 0f64..max_y)
        .map_err(|e| Error::Template(format!("chart build: {e}")))?;

    chart
        .configure_mesh()
        .y_desc("Mbps")
        .x_desc("Time (5min samples)")
        .draw()
        .map_err(|e| Error::Template(format!("chart mesh: {e}")))?;

    if !series.is_empty() {
        chart
            .draw_series(LineSeries::new(
                series.iter().enumerate().map(|(i, (_, v))| (i, *v)),
                &RED,
            ))
            .map_err(|e| Error::Template(format!("chart line: {e}")))?
            .label("Mbps")
            .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &RED));
    }

    if let Some(p) = p95 {
        if p > 0.0 {
            let blue_style: ShapeStyle = (&BLUE).into();
            chart
                .draw_series(DashedLineSeries::new(
                    (0..x_end).map(|x| (x, p)),
                    5,
                    5,
                    blue_style,
                ))
                .map_err(|e| Error::Template(format!("chart p95: {e}")))?
                .label(format!("P95 = {p:.2} Mbps"))
                .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLUE));
        }
    }

    chart
        .configure_series_labels()
        .border_style(&BLACK)
        .draw()
        .map_err(|e| Error::Template(format!("chart legend: {e}")))?;

    root.present()
        .map_err(|e| Error::Template(format!("chart present: {e}")))?;
    Ok(())
}
