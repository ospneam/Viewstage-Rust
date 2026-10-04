//! 图标级 SVG 文本解析：提取 `<svg viewBox>` 与全部 `<path>`，
//! 合并路径段（跳过 `fill="none"` 的元素）。不做完整 XML 解析——
//! 只做面向图标文件的鲁棒属性提取。

use crate::{FillRule, Icon, Paint, PaintMode, Segment};

/// SVG 文本解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgError {
    /// 找不到 `<svg` 根元素
    NoSvgRoot,
    /// 根元素缺少 viewBox
    NoViewBox,
    /// viewBox 内容非法（必须恰好 4 个数）
    BadViewBox,
    /// 没有可渲染的 `<path>`
    NoPath,
}

impl core::fmt::Display for SvgError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SvgError::NoSvgRoot => write!(f, "找不到 <svg> 根元素"),
            SvgError::NoViewBox => write!(f, "缺少 viewBox"),
            SvgError::BadViewBox => write!(f, "viewBox 必须是 4 个数"),
            SvgError::NoPath => write!(f, "没有可渲染的 <path>"),
        }
    }
}

/// 解析图标级 SVG 文本。
pub fn parse_svg(text: &str) -> Result<Icon, SvgError> {
    let attrs = next_tag(text, "<svg").ok_or(SvgError::NoSvgRoot)?.0;

    // svg 级 fill / fill-rule 作为兜底，path 级属性逐个覆盖
    let mut base = Paint::CURRENT_COLOR;
    if let Some(f) = get_attr(attrs, "fill") {
        base.mode = parse_paint_mode(f);
    }
    if let Some(r) = get_attr(attrs, "fill-rule") {
        if let Some(rule) = parse_fill_rule(r) {
            base.rule = rule;
        }
    }

    let view_box = parse_view_box(get_attr(attrs, "viewBox").ok_or(SvgError::NoViewBox)?)?;

    let mut segments: Vec<Segment> = Vec::new();
    let mut rest = text;
    let mut icon_paint = base; // 无 path 级样式时兜底；取首个可渲染 path 的样式
    let mut first_path = true;
    while let Some((path_attrs, after)) = next_tag(rest, "<path") {
        rest = after;

        let mut paint = base;
        if let Some(f) = get_attr(path_attrs, "fill") {
            paint.mode = parse_paint_mode(f);
        }
        if let Some(r) = get_attr(path_attrs, "fill-rule") {
            if let Some(rule) = parse_fill_rule(r) {
                paint.rule = rule;
            }
        }
        if paint.mode == PaintMode::None {
            continue; // fill="none" 的 path 不参与渲染
        }
        if first_path {
            icon_paint = paint;
            first_path = false;
        }
        if let Some(d) = get_attr(path_attrs, "d") {
            if let Ok(mut parsed) = crate::parse_path_data(d) {
                segments.append(&mut parsed);
            }
        }
    }

    if segments.is_empty() {
        return Err(SvgError::NoPath);
    }
    Ok(Icon {
        view_box,
        paint: icon_paint,
        segments,
    })
}

/// 在文本中查找下一个 `<tag` 标签，返回 (属性串, 闭合 `>` 之后的剩余文本)。
/// 属性串覆盖自 `<tag` 后到闭合 `>` 之前；引号内的 `>` 不视为闭合。
fn next_tag<'a>(text: &'a str, tag: &str) -> Option<(&'a str, &'a str)> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(tag) {
        let start = from + rel;
        let after = start + tag.len();
        // 边界检查：标签名后必须是空白、'>' 或 '/'（排除 `<pathx` 这类误匹配）
        match bytes.get(after) {
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') | Some(b'/') => {}
            _ => {
                from = after;
                continue;
            }
        }
        // 扫描到闭合 '>'，跳过引号内容
        let mut i = after;
        let mut quote = 0u8;
        while i < bytes.len() {
            let b = bytes[i];
            if quote != 0 {
                if b == quote {
                    quote = 0;
                }
            } else if b == b'"' || b == b'\'' {
                quote = b;
            } else if b == b'>' {
                return Some((&text[after..i], &text[i + 1..]));
            }
            i += 1;
        }
        return None; // 标签未闭合
    }
    None
}

/// 从属性串中读取指定属性值（双引号 / 单引号 / 裸值均可）。
fn get_attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // 读属性名（直到 '=' 或空白）
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let attr_name = &attrs[name_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b'=' {
            continue; // 无值属性，继续下一个
        }
        i += 1; // '='
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        match bytes.get(i) {
            Some(&q) if q == b'"' || q == b'\'' => {
                i += 1;
                let v_start = i;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                if attr_name == name {
                    return Some(&attrs[v_start..i]);
                }
                i += 1; // 结束引号
            }
            Some(_) => {
                // 裸值：读到空白
                let v_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if attr_name == name {
                    return Some(&attrs[v_start..i]);
                }
            }
            None => break,
        }
    }
    None
}

/// viewBox：逗号 / 空白分隔的 4 个数。
fn parse_view_box(s: &str) -> Result<(f32, f32, f32, f32), SvgError> {
    let mut nums = [0.0f32; 4];
    let mut idx = 0;
    for token in s.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if idx >= 4 {
            return Err(SvgError::BadViewBox);
        }
        nums[idx] = token.parse::<f32>().map_err(|_| SvgError::BadViewBox)?;
        idx += 1;
    }
    if idx != 4 {
        return Err(SvgError::BadViewBox);
    }
    Ok((nums[0], nums[1], nums[2], nums[3]))
}

/// `fill` 属性 → 填充模式。无法识别的颜色宽松回退为 currentColor。
fn parse_paint_mode(s: &str) -> PaintMode {
    match s.trim() {
        "none" => PaintMode::None,
        "" | "currentColor" | "currentcolor" => PaintMode::CurrentColor,
        other => parse_hex_color(other)
            .map(PaintMode::Solid)
            .unwrap_or(PaintMode::CurrentColor),
    }
}

/// `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa`。
fn parse_hex_color(s: &str) -> Option<[f32; 4]> {
    let hex = s.strip_prefix('#')?;
    let ch2 = |i: usize| -> Option<u8> { u8::from_str_radix(&hex[i..i + 2], 16).ok() };
    let (r, g, b, a) = match hex.len() {
        3 => (
            u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?,
            u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?,
            u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?,
            255,
        ),
        6 => (ch2(0)?, ch2(2)?, ch2(4)?, 255),
        8 => (ch2(0)?, ch2(2)?, ch2(4)?, ch2(6)?),
        _ => return None,
    };
    Some([
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ])
}

fn parse_fill_rule(s: &str) -> Option<FillRule> {
    match s.trim() {
        "nonzero" => Some(FillRule::NonZero),
        "evenodd" => Some(FillRule::EvenOdd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Segment::{Close, LineTo, MoveTo};

    #[test]
    fn typical_current_color_icon() {
        // 复制自 assets 图标的典型结构：svg fill=none + path fill=currentColor
        let text = r#"<svg width="24" height="24" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M2 5.75C2 5.34 2.34 5 2.75 5h18.5c.41 0 .75.34.75.75S21.66 6.5 21.25 6.5H2.75C2.34 6.5 2 6.16 2 5.75Z" fill="currentColor"/></svg>"#;
        let icon = parse_svg(text).unwrap();
        assert_eq!(icon.view_box, (0.0, 0.0, 24.0, 24.0));
        assert_eq!(icon.paint.mode, PaintMode::CurrentColor);
        assert_eq!(icon.paint.rule, FillRule::NonZero);
        assert!(matches!(icon.segments[0], MoveTo(2.0, 5.75)));
    }

    #[test]
    fn evenodd_is_propagated() {
        let text = r#"<svg viewBox="0 0 24 24"><path fill-rule="evenodd" d="M0 0L24 0L0 24Z M0 0L0 24L24 0Z"/></svg>"#;
        let icon = parse_svg(text).unwrap();
        assert_eq!(icon.paint.rule, FillRule::EvenOdd);
    }

    #[test]
    fn none_paths_are_skipped() {
        let text = r##"<svg viewBox="0 0 24 24"><path d="M1 1L2 2" fill="none"/><path d="M0 0L5 5Z" fill="#212121"/></svg>"##;
        let icon = parse_svg(text).unwrap();
        assert_eq!(icon.segments, vec![MoveTo(0.0, 0.0), LineTo(5.0, 5.0), Close]);
        match icon.paint.mode {
            PaintMode::Solid(c) => {
                // 0x21 = 33
                let v = 33.0 / 255.0;
                assert!((c[0] - v).abs() < 1e-4 && (c[1] - v).abs() < 1e-4);
            }
            other => panic!("expected solid, got {other:?}"),
        }
    }

    #[test]
    fn multiple_paths_merge() {
        let text = r#"<svg viewBox="0 0 24 24"><path d="M0 0L1 1"/><path d="M2 2L3 3"/><path d="M4 4Z"/></svg>"#;
        let icon = parse_svg(text).unwrap();
        // 每条 path 各产生 MoveTo + 一段 = 6 段
        assert_eq!(icon.segments.len(), 6);
    }

    #[test]
    fn svg_level_fill_rule_fallback() {
        let text = r#"<svg viewBox="0 0 24 24" fill-rule="evenodd"><path d="M0 0L1 1"/></svg>"#;
        let icon = parse_svg(text).unwrap();
        assert_eq!(icon.paint.rule, FillRule::EvenOdd);
    }

    #[test]
    fn comments_and_ignored_tags() {
        let text = r#"<?xml version="1.0"?><!-- icon --><svg viewBox="0 0 24 24"><title>x</title><path d="M0 0L1 1"/></svg>"#;
        let icon = parse_svg(text).unwrap();
        assert_eq!(icon.segments.len(), 2);
    }

    #[test]
    fn errors() {
        assert_eq!(parse_svg("<path d='M0 0L1 1'/>"), Err(SvgError::NoSvgRoot));
        assert_eq!(parse_svg("<svg></svg>"), Err(SvgError::NoViewBox));
        assert_eq!(parse_svg("<svg><path d='M0 0'/></svg>"), Err(SvgError::NoViewBox));
        assert_eq!(
            parse_svg("<svg viewBox='0 0 24'><path d='M0 0'/></svg>"),
            Err(SvgError::BadViewBox)
        );
        assert_eq!(
            parse_svg("<svg viewBox='0 0 24 24'><rect/></svg>"),
            Err(SvgError::NoPath)
        );
    }

    #[test]
    fn gt_inside_quotes_is_not_tag_end() {
        // 属性值内含 '>' 时标签解析不提前截断
        let text = r#"<svg viewBox="0 0 24 24"><path data-x="a>b" d="M0 0L1 1"/></svg>"#;
        let icon = parse_svg(text).unwrap();
        assert_eq!(icon.segments.len(), 2);
    }
}
