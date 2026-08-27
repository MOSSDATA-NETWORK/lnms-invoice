#!/usr/bin/env python3
"""汇总预检报告:对每个模板给出 umya-spreadsheet 兼容性相关的关键指标。"""
import re
import sys
from pathlib import Path

# 复用 audit.py
sys.path.insert(0, str(Path(__file__).parent))
from audit import audit  # noqa: E402


def summarize(report: dict) -> str:
    out = []
    out.append(f"# 模板预检汇总:{Path(report['file']).name}")
    out.append(f"- 文件大小:{report['size_bytes']:,} bytes")
    out.append(f"- Sheet 数量:{len(report['sheets'])}")
    out.append(f"- 命名区域(Defined Names):{len(report.get('defined_names', []))}")
    if report.get("defined_names"):
        for n in report["defined_names"][:5]:
            out.append(f"  - `{n['name']}` → {n['destinations']}")
    out.append("")

    for s in report["sheets"]:
        out.append(f"## Sheet: `{s['name']}` (state={s['state']}, dim={s['dimensions']})")
        out.append(f"- 合并单元格:{len(s['merged_cells'])} 个")
        for m in s["merged_cells"]:
            out.append(f"  - {m}")
        out.append(f"- 公式数量:**{len(s['formulas'])}**")
        for f in s["formulas"][:10]:
            out.append(f"  - `{f['cell']}` = `{f['formula'][:60]}`")
        if len(s["formulas"]) > 10:
            out.append(f"  - ...(其余 {len(s['formulas']) - 10} 个省略)")
        out.append(f"- 条件格式:{len(s['conditional_formatting'])} 条")
        for cf in s["conditional_formatting"]:
            types = "/".join(cf.get("rule_types", []))
            out.append(f"  - range={cf['range']}, types={types}")
        out.append(f"- 数据验证:{len(s['data_validations'])} 条")
        for dv in s["data_validations"]:
            out.append(f"  - type={dv['type']}, formula1={dv['formula1']}, range={dv['ranges']}")
        out.append(f"- 打印区域:`{s['print_area']}`")
        out.append(f"- 打印标题:`{s['print_titles']}`")
        out.append(f"- 页面方向:{s['page_setup'].get('orientation')}, paperSize={s['page_setup'].get('paperSize')}, fitToWidth={s['page_setup'].get('fitToWidth')}")
        out.append(f"- 边距: L={s['page_margins']['left']} R={s['page_margins']['right']} T={s['page_margins']['top']} B={s['page_margins']['bottom']}")
        out.append(f"- 页眉/页脚: H=`{s['header_footer']['oddHeader']}` / F=`{s['header_footer']['oddFooter']}`")
        out.append(f"- 图表:{s['charts_count']} 个 | 图片:{s['images_count']} 个 | Tables:{s['tables']}")
        out.append(f"- 冻结窗格:`{s['freeze_panes']}`, 缩放:{s['sheet_view_zoom']}")
        out.append(f"- 显式行高(自定义):{len(s['row_heights_sample'])} 个")
        out.append(f"- 显式列宽(自定义):{len(s['col_widths_sample'])} 个")
        out.append(f"- 字体使用: {', '.join(s['font_names_used']) or '(无)'}")
        out.append(f"- 填充色使用: {', '.join(s['fill_colors_used'][:10]) or '(无)'}")
        out.append(f"- 数字格式去重: {len(s['number_formats_used'])} 种")
        for nf in s["number_formats_used"][:15]:
            out.append(f"  - `{nf}`")
        if len(s["number_formats_used"]) > 15:
            out.append(f"  - ...(其余 {len(s['number_formats_used']) - 15} 种)")
        out.append("")
        out.append("### 非空单元格样本(前 60 个,占位符线索)")
        for c in s["non_empty_cells_sample"]:
            v = c["value"]
            tag = " 🟢疑似占位符" if isinstance(v, str) and re.search(r"[\{\}\$]|XX|TODO|示例|sample|placeholder", v, re.IGNORECASE) else ""
            out.append(f"- `{c['cell']}` = `{v}` (fmt=`{c['number_format']}`){tag}")
        out.append("")
    return "\n".join(out)


if __name__ == "__main__":
    for p in sys.argv[1:]:
        r = audit(Path(p))
        print(summarize(r))
        print("=" * 80)
