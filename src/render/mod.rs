//! 账单渲染管线(阶段 5)
//!
//! 三步:
//! 1. `fill_template`:umya-spreadsheet 写数据(INVOICE NO / 客户名 / 期间 / 端口行 / 合计 / 95th)
//! 2. `replace_media_in_xlsx`:zip 改写,字节级替换 `xl/media/<file>`(决策 #9,
//!    保留 drawing XML 不变)
//! 3. `xlsx_to_pdf`:soffice --headless --convert-to pdf(独立 UserInstallation 隔离)
//!
//! 上层 `InvoiceRenderer::generate_preview` 编排这三步 + 临时文件原子 rename 到
//! `pdf_path_preview`。

pub mod fill;
pub mod pdf;
pub mod png;

pub use fill::{fill_template, InvoiceData, PortLine};
pub use pdf::xlsx_to_pdf;
pub use png::replace_media_in_xlsx;
