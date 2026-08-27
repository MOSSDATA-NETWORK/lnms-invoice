//! 模板填充(阶段 5)
//!
//! 把 `InvoiceData` 写入目标 sheet 的固定单元格;不破坏原模板的样式/公式/drawing。
//!
//! 单元格地址通过决策 #20 的 `cell_map` 决定:
//! - 实际项目里这部分由客户化配置(每个客户一份 mapping JSON)给出,
//!   阶段 5 先用硬编码 + 通用约定地址,阶段 6 接入 customer_config 表。

use crate::error::{Error, Result};
use std::path::Path;
use umya_spreadsheet::structs::Worksheet;

/// 账单填充数据(简化版,阶段 6 接入完整 InvoiceSnapshot)
#[derive(Debug, Clone)]
pub struct InvoiceData {
    pub invoice_no: String,
    pub customer_name: String,
    pub period_label: String,    // "2026-08" 类
    pub ports: Vec<PortLine>,
    pub total_yuan: f64,
    pub currency: String,
}

#[derive(Debug, Clone)]
pub struct PortLine {
    pub label: String,
    pub mbps_95th: Option<i64>,
    pub machine_rent: bool,
    pub machine_hosting: bool,
}

/// 把 InvoiceData 写入 xlsx 副本。
/// - `template_path`:模板文件(只读)
/// - `output_path`:输出 xlsx(覆盖若存在)
/// - `sheet_name`:目标 sheet 名(模板里应有)
/// - `data`:填充数据
pub fn fill_template(
    template_path: &Path,
    output_path: &Path,
    sheet_name: &str,
    data: &InvoiceData,
) -> Result<()> {
    let mut book = umya_spreadsheet::reader::xlsx::read(template_path)
        .map_err(|e| Error::Template(format!("open template: {e}")))?;

    let sheet = book
        .sheet_by_name_mut(sheet_name)
        .map_err(|e| Error::Template(format!("locate sheet '{sheet_name}': {e}")))?;

    write_invoice_fields(sheet, data)?;
    write_port_rows(sheet, &data.ports)?;

    umya_spreadsheet::writer::xlsx::write(&book, output_path)
        .map_err(|e| Error::Template(format!("write filled xlsx: {e}")))?;

    Ok(())
}

/// 写 INVOICE NO / 客户名 / 期间 / 合计 / 币种到约定单元格。
///
/// 约定地址(决策 #20 的硬编码 fallback,生产由 cell_map 配置覆盖):
///   D2 = INVOICE NO
///   D3 = 客户名
///   D4 = 期间(如 "2026-08")
///   D5 = 合计(数字,分)
///   D6 = 币种
fn write_invoice_fields(sheet: &mut Worksheet, data: &InvoiceData) -> Result<()> {
    sheet.cell_mut("D2").set_value(&data.invoice_no);
    sheet.cell_mut("D3").set_value(&data.customer_name);
    sheet.cell_mut("D4").set_value(&data.period_label);
    sheet.cell_mut("D5").set_value_number(data.total_yuan);
    sheet.cell_mut("D6").set_value(&data.currency);
    Ok(())
}

/// 写端口行(从第 9 行起,逐行)。
///
/// 约定列:
///   A = port label
///   B = 95th Mbps(整数)
///   C = (legacy) IP A 段数 — v0.6.3 起 IP 数量在 rates.ip_quantity 直填,
///       这里写 blank,模板如需保留该单元格可继续使用
///   D = (legacy) IP B 段数 — 同上,写 blank
///   E = machine rent
///   F = machine hosting
///
/// 注意:IP 总费用合计到 D5(InvoiceData.total_yuan 元,f64),由 run-billing
/// 在 build_invoice_lines 用 rate.ip_quantity × rate.ip_unit_price_yuan 计算后并入。
/// 模板若希望把 IP 费用作为单独行显示,需要单独修模板(超出本仓库范围)。
fn write_port_rows(sheet: &mut Worksheet, ports: &[PortLine]) -> Result<()> {
    const FIRST_ROW: u32 = 9;
    for (i, p) in ports.iter().enumerate() {
        let row = FIRST_ROW + i as u32;
        let r = row.to_string();
        sheet.cell_mut(format!("A{r}")).set_value(&p.label);
        if let Some(m) = p.mbps_95th {
            sheet.cell_mut(format!("B{r}")).set_value_number(m as f64);
        } else {
            sheet.cell_mut(format!("B{r}")).set_blank();
        }
        sheet.cell_mut(format!("C{r}")).set_blank();
        sheet.cell_mut(format!("D{r}")).set_blank();
        sheet
            .cell_mut(format!("E{r}"))
            .set_value_bool(p.machine_rent);
        sheet
            .cell_mut(format!("F{r}"))
            .set_value_bool(p.machine_hosting);
    }
    Ok(())
}
