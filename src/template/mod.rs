//! 模板预检与填充(阶段 3/4.5/5)。
//!
//! - 阶段 3(本文件):读取 xlsx,产出 SHA256 + 单元格清单 + drawing 锚点 + 媒体清单;
//!   结果落 `template_versions` 表(决策 #20 数据血缘)。
//! - 阶段 4.5:plotters 渲染 95th 曲线 PNG(2069×713,模板 picture 原始尺寸)。
//! - 阶段 5:umya-spreadsheet 写数据 + OOXML 字节级替换 `xl/media/image1.png` +
//!   soffice 转 PDF。

pub mod audit;
pub mod inspect;

pub use audit::write_template_version;
pub use inspect::{
    column_letter, inspect, parse_drawing_anchors, CellEntry, CellKind, DrawingEntry, MediaEntry,
    TemplateAudit,
};
