//! 阶段 3 模板预检单元测试
//!
//! 用 umya 现场构造一个最小 xlsx(单 sheet + 几个单元格 + 一张 PNG 媒体),
//! 然后 inspect(),断言:
//! - SHA256 与文件内容一致
//! - sheet 名 + 单元格清单 + 类型分类正确
//! - 媒体清单能拿到 PNG 尺寸
//! - 落 template_versions 表可读回

use lnms_invoice::store::Store;
use lnms_invoice::template::{inspect, write_template_version, CellKind};
use std::io::Write;
use tempfile::tempdir;
use umya_spreadsheet::Workbook;

fn write_minimal_xlsx(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("minimal.xlsx");
    let mut book = Workbook::default();

    let sheet = book.new_sheet("Invoice").expect("new sheet");
    sheet.cell_mut("A1").set_value("湖南XX网络");
    sheet.cell_mut("B1").set_value_number(100);
    sheet.cell_mut("C1").set_formula("SUM(B1:B10)");

    umya_spreadsheet::writer::xlsx::write(&book, &path).expect("write xlsx");

    // 注入最小 PNG 到 xl/media/image1.png
    inject_minimal_png(&path);

    path
}

/// 直接向 zip 注入 1x1 PNG 到 xl/media/image1.png
fn inject_minimal_png(xlsx_path: &std::path::Path) {
    let png_1x1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let tmp = xlsx_path.with_extension("tmp.xlsx");
    {
        let src = std::fs::File::open(xlsx_path).unwrap();
        let mut zip_in = zip::ZipArchive::new(std::io::BufReader::new(src)).unwrap();
        let dst = std::fs::File::create(&tmp).unwrap();
        let mut zip_out = zip::ZipWriter::new(dst);

        let opts = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for i in 0..zip_in.len() {
            let mut entry = zip_in.by_index(i).unwrap();
            let name = entry.name().to_string();
            zip_out.start_file(&name, opts).unwrap();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
            zip_out.write_all(&buf).unwrap();
        }

        zip_out
            .start_file("xl/media/image1.png", opts)
            .unwrap();
        zip_out.write_all(png_1x1).unwrap();

        zip_out.finish().unwrap();
    }
    std::fs::rename(&tmp, xlsx_path).unwrap();
}

#[test]
fn test_inspect_minimal_xlsx() {
    let dir = tempdir().unwrap();
    let xlsx = write_minimal_xlsx(dir.path());

    let audit = inspect(&xlsx, "minimal").expect("inspect");

    eprintln!("sheets: {:?}", audit.sheets);
    eprintln!("cell_map: {:#?}", audit.cell_map);

    assert_eq!(audit.template_name, "minimal");
    assert_eq!(audit.sha256.len(), 64);
    assert!(!audit.sheets.is_empty());
    assert!(audit.sheets.iter().any(|s| s == "Invoice"));

    let a1 = audit
        .cell_map
        .iter()
        .find(|c| c.sheet == "Invoice" && c.cell == "A1")
        .expect("A1 应存在");
    assert_eq!(a1.kind, CellKind::Text);
    assert_eq!(a1.value.as_deref(), Some("湖南XX网络"));

    let b1 = audit
        .cell_map
        .iter()
        .find(|c| c.sheet == "Invoice" && c.cell == "B1")
        .expect("B1 应存在");
    assert_eq!(b1.kind, CellKind::Number);
    assert_eq!(b1.value.as_deref(), Some("100"));

    let c1 = audit
        .cell_map
        .iter()
        .find(|c| c.sheet == "Invoice" && c.cell == "C1")
        .expect("C1 应存在");
    assert_eq!(c1.kind, CellKind::Formula);
    assert_eq!(c1.formula.as_deref(), Some("SUM(B1:B10)"));

    assert_eq!(audit.media.len(), 1);
    assert_eq!(audit.media[0].media_path, "xl/media/image1.png");
    assert_eq!(audit.media[0].width, Some(1));
    assert_eq!(audit.media[0].height, Some(1));
}

#[tokio::test]
async fn test_write_template_version_roundtrip() {
    let dir = tempdir().unwrap();
    let xlsx = write_minimal_xlsx(dir.path());
    let audit = inspect(&xlsx, "minimal-roundtrip").expect("inspect");

    let db = dir.path().join("t.sqlite");
    let store = Store::connect(&db).await.expect("store");

    write_template_version(&store, &audit)
        .await
        .expect("write");

    let row: (String, String, String, String, String) = sqlx::query_as(
        "SELECT template_name, template_sha256, cell_map_json, drawing_anchors_json, last_validated_at
         FROM template_versions WHERE template_name = ?",
    )
    .bind("minimal-roundtrip")
    .fetch_one(store.pool())
    .await
    .expect("fetch");
    assert_eq!(row.0, "minimal-roundtrip");
    assert_eq!(row.1, audit.sha256);
    assert!(row.2.contains("湖南XX网络"));
    assert!(row.2.contains("SUM(B1:B10)"));

    write_template_version(&store, &audit)
        .await
        .expect("rewrite");
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM template_versions WHERE template_name = ?",
    )
    .bind("minimal-roundtrip")
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn test_column_letter_edges() {
    use lnms_invoice::template::inspect::column_letter;
    assert_eq!(column_letter(0), "A");
    assert_eq!(column_letter(25), "Z");
    assert_eq!(column_letter(26), "AA");
    assert_eq!(column_letter(27), "AB");
    assert_eq!(column_letter(701), "ZZ");
    assert_eq!(column_letter(702), "AAA");
}

#[test]
fn test_parse_drawing_anchors_two_cell() {
    use lnms_invoice::template::parse_drawing_anchors;

    let xml = r#"<?xml version="1.0"?>
    <xdr:wsDr xmlns:xdr="http://x">
      <xdr:twoCellAnchor>
        <xdr:from><xdr:col>5</xdr:col><xdr:row>19</xdr:row></xdr:from>
        <xdr:to><xdr:col>19</xdr:col><xdr:row>38</xdr:row></xdr:to>
      </xdr:twoCellAnchor>
      <xdr:oneCellAnchor>
        <xdr:from><xdr:col>0</xdr:col><xdr:row>0</xdr:row></xdr:from>
      </xdr:oneCellAnchor>
    </xdr:wsDr>"#;

    let anchors = parse_drawing_anchors(xml).expect("parse");
    assert_eq!(anchors.len(), 2);

    // twoCellAnchor
    assert_eq!(anchors[0].from, Some((5, 19)));
    assert_eq!(anchors[0].to, Some((19, 38)));

    // oneCellAnchor
    assert_eq!(anchors[1].from, Some((0, 0)));
    assert_eq!(anchors[1].to, None);
}
