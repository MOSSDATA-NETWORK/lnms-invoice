//! 模板预检(阶段 3)。
//!
//! 用 umya-spreadsheet 3.1.0 读取 xlsx 的 sheet/单元格/formula/drawing 锚点;
//! 用 zip 0.6 直读 xlsx 内部 `xl/media/*` 算 SHA256 + 像素尺寸;
//! 用 sha2 算模板整体 SHA256。
//!
//! 输出 `TemplateAudit`,供 `audit::write_template_version` 落表,
//! 也供阶段 5 填表时按 drawing 锚点 + 媒体路径做 PNG 字节替换。

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use umya_spreadsheet::reader::xlsx as umya_reader;
use umya_spreadsheet::structs::Worksheet;

/// 单个模板的完整预检结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TemplateAudit {
    pub template_name: String,
    pub sha256: String,
    pub bytes: u64,
    pub sheets: Vec<String>,
    pub cell_map: Vec<CellEntry>,
    pub drawings: Vec<DrawingEntry>,
    pub media: Vec<MediaEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CellEntry {
    pub sheet: String,
    /// "A1" 形式(字母列 + 1-based 行号)
    pub cell: String,
    pub kind: CellKind,
    /// umya 原 data_type,典型值 "s"/"str"/"inlineStr"/"n"/"b"/"f"
    pub data_type: String,
    pub value: Option<String>,
    pub formula: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    Text,
    Number,
    Formula,
    Boolean,
    Empty,
    Other,
}

/// drawing 锚点(目前阶段 5 替换 PNG 用 from_cell 即可)。
/// 媒体路径阶段 5 实装替换时直接用 `media` 列表里的 `xl/media/image1.png`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DrawingEntry {
    pub sheet: String,
    pub from_cell: Option<String>,
    pub to_cell: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaEntry {
    pub media_path: String,
    pub sha256: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub fn inspect(path: &Path, template_name: &str) -> Result<TemplateAudit> {
    let bytes = std::fs::read(path)?;
    let sha256 = hex_sha256(&bytes);
    let book = umya_reader::read(path).map_err(|e| Error::Template(format!("open xlsx: {e}")))?;

    let sheets: Vec<String> = book
        .sheet_collection()
        .iter()
        .map(|s| s.name().to_string())
        .collect();

    let mut cell_map = Vec::new();
    for sheet in book.sheet_collection() {
        collect_cells(sheet, &mut cell_map);
    }

    let drawings = collect_drawings(path, book.sheet_collection())?;
    let media = collect_media(path)?;

    Ok(TemplateAudit {
        template_name: template_name.to_string(),
        sha256,
        bytes: bytes.len() as u64,
        sheets,
        cell_map,
        drawings,
        media,
    })
}

fn collect_cells(sheet: &Worksheet, out: &mut Vec<CellEntry>) {
    let sheet_name = sheet.name().to_string();
    for cell in sheet.cells() {
        let coord = cell.coordinate();
        let cell_addr = format!(
            "{}{}",
            // umya 的 col_num / row_num 都是 1-based(Excel 内部约定),
            // 列字母要从 0-based 转,行号直接用。
            column_letter(coord.col_num() as i32 - 1),
            coord.row_num()
        );
        let value = cell.value().into_owned();
        let formula = cell.formula().to_string();
        let dt = cell.data_type().to_string();
        let kind = classify(&dt, &value, &formula);
        if matches!(kind, CellKind::Empty) {
            continue;
        }
        out.push(CellEntry {
            sheet: sheet_name.clone(),
            cell: cell_addr,
            kind,
            data_type: dt,
            value: if value.is_empty() {
                None
            } else {
                Some(value)
            },
            formula: if formula.is_empty() {
                None
            } else {
                Some(formula)
            },
        });
    }
}

fn classify(dt: &str, value: &str, formula: &str) -> CellKind {
    if !formula.is_empty() {
        return CellKind::Formula;
    }
    match dt {
        "s" | "str" | "inlineStr" => CellKind::Text,
        "n" => CellKind::Number,
        "b" => CellKind::Boolean,
        _ if value.is_empty() => CellKind::Empty,
        _ => CellKind::Other,
    }
}

fn collect_drawings(xlsx_path: &Path, sheets: &[Worksheet]) -> Result<Vec<DrawingEntry>> {
    // umya 3.x 不在 reader 自动 parse drawing 到 worksheet_drawing;
    // 直接 zip + quick-xml 解 drawing*.xml + worksheet rels。
    let f = File::open(xlsx_path)?;
    let mut zip =
        zip::ZipArchive::new(BufReader::new(f)).map_err(|e| Error::Template(format!("{e}")))?;

    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).map(|e| e.name().to_string()))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Template(format!("list zip: {e}")))?;

    // 1. 收所有 drawing*.xml 内容
    let mut drawing_xmls: Vec<(String, String)> = Vec::new();
    for n in &names {
        if n.starts_with("xl/drawings/drawing") && n.ends_with(".xml") {
            let bytes = read_zip_entry(&mut zip, n)?;
            let xml = String::from_utf8_lossy(&bytes).to_string();
            drawing_xmls.push((n.clone(), xml));
        }
    }
    let mut anchors_by_drawing: std::collections::HashMap<String, Vec<Anchor>> =
        std::collections::HashMap::new();
    for (path, xml) in &drawing_xmls {
        let key = path.rsplit('/').next().unwrap_or(path).to_string();
        anchors_by_drawing.insert(key, parse_drawing_anchors(xml)?);
    }

    // 2. 找每个 sheet 的 drawing target
    //    路径 xl/worksheets/_rels/sheet{N}.xml.rels,内容里 Target="../drawings/drawing{N}.xml"
    let mut sheet_to_drawing: Vec<(usize, String)> = Vec::new();
    for n in &names {
        if !n.starts_with("xl/worksheets/_rels/sheet") || !n.ends_with(".xml.rels") {
            continue;
        }
        let sheet_idx = extract_sheet_index(n).unwrap_or(0);
        let bytes = read_zip_entry(&mut zip, n)?;
        let xml = String::from_utf8_lossy(&bytes).to_string();
        if let Some(target) = extract_drawing_target(&xml) {
            // Target="drawings/drawingN.xml" 或 "../drawings/drawingN.xml"
            let key = target.rsplit('/').next().unwrap_or(&target).to_string();
            sheet_to_drawing.push((sheet_idx, key));
        }
    }

    // 3. 组合:从 sheet index 拿 sheet name(sheets 按 workbook.xml 顺序,index 1-based)
    let mut out = Vec::new();
    for (sheet_idx, drawing_key) in sheet_to_drawing {
        let sheet_name = sheets
            .get(sheet_idx.saturating_sub(1))
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        if let Some(anchors) = anchors_by_drawing.get(&drawing_key) {
            for a in anchors {
                // OOXML drawing col/row 是 0-based;列字母直接转,行 +1 显示
                out.push(DrawingEntry {
                    sheet: sheet_name.clone(),
                    from_cell: a
                        .from
                        .map(|(c, r)| format!("{}{}", column_letter(c as i32), r + 1)),
                    to_cell: a
                        .to
                        .map(|(c, r)| format!("{}{}", column_letter(c as i32), r + 1)),
                });
            }
        }
    }
    Ok(out)
}

/// `(col, row)`,均为 0-based(OOXML drawing XML 内部约定)
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    pub from: Option<(u32, u32)>,
    pub to: Option<(u32, u32)>,
}

/// 用 quick-xml 扫 drawing XML,识别 `<xdr:twoCellAnchor>` /
/// `<xdr:oneCellAnchor>` 块,提取其中 `<xdr:from>` / `<xdr:to>` 的
/// `<xdr:col>` 和 `<xdr:row>` 数值。
///
/// 不追求完整解析 OOXML,只覆盖 LibreNMS 95th 图表 + 一般图片的 drawing 形态。
pub fn parse_drawing_anchors(xml: &str) -> Result<Vec<Anchor>> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    // quick-xml 0.31 默认不 trim text,但 drawing XML 里 <xdr:col>0</xdr:col>
    // 不会出现空白,所以不强制 trim。

    let mut anchors: Vec<Anchor> = Vec::new();
    let mut current: Option<Anchor> = None;
    let mut in_marker: Option<&'static str> = None;
    let mut pending_col: Option<u32> = None;
    let mut pending_row: Option<u32> = None;
    let mut text_buf = String::new();

    let mut buf: Vec<u8> = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = bytes_start_local(&e);
                match local.as_deref() {
                    Some("twoCellAnchor") | Some("oneCellAnchor") => {
                        current = Some(Anchor { from: None, to: None });
                    }
                    Some("from") | Some("to") => {
                        in_marker = if local.as_deref() == Some("from") {
                            Some("from")
                        } else {
                            Some("to")
                        };
                        pending_col = None;
                        pending_row = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                if in_marker.is_some() {
                    text_buf.clear();
                    text_buf.push_str(&e.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(e)) => {
                let local = bytes_end_local(&e);
                match local.as_deref() {
                    Some("col") => {
                        if in_marker.is_some() {
                            pending_col = text_buf.trim().parse().ok();
                        }
                    }
                    Some("row") => {
                        if in_marker.is_some() {
                            pending_row = text_buf.trim().parse().ok();
                        }
                    }
                    Some("from") | Some("to") => {
                        if let (Some(c), Some(r)) = (pending_col.take(), pending_row.take()) {
                            if let Some(a) = current.as_mut() {
                                if local.as_deref() == Some("from") {
                                    a.from = Some((c, r));
                                } else {
                                    a.to = Some((c, r));
                                }
                            }
                        }
                        in_marker = None;
                        text_buf.clear();
                    }
                    Some("twoCellAnchor") | Some("oneCellAnchor") => {
                        if let Some(a) = current.take() {
                            anchors.push(a);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::Template(format!("drawing xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(anchors)
}

fn bytes_start_local(e: &quick_xml::events::BytesStart) -> Option<String> {
    let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    if raw.is_empty() {
        return None;
    }
    let local = raw.rsplit(':').next().unwrap_or(&raw).to_string();
    Some(local)
}

fn bytes_end_local(e: &quick_xml::events::BytesEnd) -> Option<String> {
    let raw = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    if raw.is_empty() {
        return None;
    }
    let local = raw.rsplit(':').next().unwrap_or(&raw).to_string();
    Some(local)
}

fn extract_sheet_index(rels_path: &str) -> Option<usize> {
    // "xl/worksheets/_rels/sheet3.xml.rels" → 3
    let fname = rels_path.rsplit('/').next()?;
    let num = fname
        .strip_prefix("sheet")?
        .strip_suffix(".xml.rels")?;
    num.parse().ok()
}

fn extract_drawing_target(rels_xml: &str) -> Option<String> {
    // 找 Type 含 "/drawing" 的 Relationship 的 Target
    let mut in_rel = false;
    let mut rel_type = String::new();
    let mut rel_target = String::new();
    for line in rels_xml.lines() {
        let l = line.trim();
        if l.starts_with("<Relationship") {
            in_rel = true;
            rel_type.clear();
            rel_target.clear();
            // 属性抽取(简单 split)
            for (_i, _part) in l.split('"').enumerate() {
                // 偶数索引是属性名 / 奇数是值
                // 不优雅但能跑:找 Type="...drawing..." Target="..."
            }
            // 用更简单的方式:看是否含 "drawing" 且含 Target
            if l.contains("drawing") {
                if let Some(t_start) = l.find("Target=\"") {
                    let after = &l[t_start + 8..];
                    if let Some(t_end) = after.find('"') {
                        return Some(after[..t_end].to_string());
                    }
                }
            }
        }
    }
    let _ = (in_rel, rel_type, rel_target);
    None
}

fn read_zip_entry(zip: &mut zip::ZipArchive<BufReader<File>>, name: &str) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .map_err(|e| Error::Template(format!("open {name}: {e}")))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    std::io::Read::read_to_end(&mut entry, &mut buf)
        .map_err(|e| Error::Template(format!("read {name}: {e}")))?;
    Ok(buf)
}

fn collect_media(xlsx_path: &Path) -> Result<Vec<MediaEntry>> {
    let f = File::open(xlsx_path)?;
    let mut zip =
        zip::ZipArchive::new(BufReader::new(f)).map_err(|e| Error::Template(format!("{e}")))?;

    // 先收集 names(zip borrow 规则)
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).map(|e| e.name().to_string()))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Template(format!("list zip: {e}")))?;

    let mut media = Vec::new();
    for name in names {
        if !name.starts_with("xl/media/") {
            continue;
        }
        let mut entry = zip
            .by_name(&name)
            .map_err(|e| Error::Template(format!("open {name}: {e}")))?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| Error::Template(format!("read {name}: {e}")))?;
        let sha = hex_sha256(&buf);
        let (w, h) = match image::ImageReader::new(std::io::Cursor::new(&buf))
            .with_guessed_format()
        {
            Ok(r) => match r.into_dimensions() {
                Ok((w, h)) => (Some(w), Some(h)),
                Err(e) => {
                    log::warn!("image dimensions for {name}: {e}");
                    (None, None)
                }
            },
            Err(e) => {
                log::warn!("image format guess for {name}: {e}");
                (None, None)
            }
        };
        media.push(MediaEntry {
            media_path: name,
            sha256: sha,
            bytes: buf.len() as u64,
            width: w,
            height: h,
        });
    }
    Ok(media)
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 0-based 列号 → 字母("A" = 0, "Z" = 25, "AA" = 26)
pub fn column_letter(mut col_0: i32) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + col_0.rem_euclid(26) as u8) as char);
        col_0 = col_0 / 26 - 1;
        if col_0 < 0 {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_letter_basic() {
        assert_eq!(column_letter(0), "A");
        assert_eq!(column_letter(25), "Z");
        assert_eq!(column_letter(26), "AA");
        assert_eq!(column_letter(27), "AB");
        assert_eq!(column_letter(701), "ZZ");
        assert_eq!(column_letter(702), "AAA");
    }
}
