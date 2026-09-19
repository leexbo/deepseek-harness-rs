//! PDF 渲染器(pdf-syntax 解析 + pdf-render/vello_cpu 纯 Rust 栅格化,
//! 无 C 交付物)。
//!
//! 每次渲染按需重开文档(解析器自带 xref/页树恢复,容错友好);页
//! 尺寸可未栅格化先取(`Page::render_dimensions`),供布局占位与
//! 宽高比。栅格产物为预乘 RGBA,此处转直通 RGBA8(gpui RenderImage
//! 的输入形态)。渲染目标宽取固定逻辑宽(面板 540–800,2x 采样
//! 兼顾高分屏),超出由显示端等比收窄。

use pdf_render::pdf_interpret::InterpreterSettings;
use pdf_render::pdf_syntax::{LoadPdfError, Pdf};
use pdf_render::vello_cpu::color::palette::css::WHITE;

/// 目标渲染宽(逻辑 px)
const RENDER_WIDTH: f32 = 1000.;

/// 一页栅格产物(直通 RGBA8)
pub struct PdfPageImage {
    /// 像素宽
    pub width: u32,
    /// 像素高
    pub height: u32,
    /// 直通 RGBA8 位图
    pub rgba: Vec<u8>,
}

/// PDF 处理错误(密码/无效;文案见 views 的映射)
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PdfError {
    /// 加密文档(暂不支持预览)
    Password,
    /// 解析或渲染失败(带消息)
    Invalid(String),
}

fn map_load_err(err: LoadPdfError) -> PdfError {
    match err {
        LoadPdfError::Decryption(_) => PdfError::Password,
        LoadPdfError::Invalid => PdfError::Invalid("无法解析这份 PDF".to_string()),
        LoadPdfError::TooLarge(objects, pages) => {
            PdfError::Invalid(format!("PDF 过大(对象 {objects} / 页 {pages})"))
        }
    }
}

/// 打开文档取各页渲染尺寸(未栅格化;宽高比占位用)
pub fn open_page_dims(bytes: &[u8]) -> Result<Vec<(f32, f32)>, PdfError> {
    let doc = Pdf::new(bytes.to_vec()).map_err(map_load_err)?;
    Ok(doc.pages().iter().map(|p| p.render_dimensions()).collect())
}

/// 渲染一页(目标宽 [`RENDER_WIDTH`] 等比;白底)
pub fn render_page(bytes: &[u8], page_ix: usize) -> Result<PdfPageImage, PdfError> {
    let doc = Pdf::new(bytes.to_vec()).map_err(map_load_err)?;
    let page = doc
        .pages()
        .get(page_ix)
        .ok_or_else(|| PdfError::Invalid(format!("没有第 {} 页", page_ix + 1)))?;
    let (w_pt, _h_pt) = page.render_dimensions();
    let scale = RENDER_WIDTH / w_pt.max(1.);
    let settings = pdf_render::RenderSettings {
        x_scale: scale,
        y_scale: scale,
        width: None,
        height: None,
        bg_color: WHITE,
        ..Default::default()
    };
    let pixmap = pdf_render::render(page, &InterpreterSettings::default(), &settings);
    let (w, h) = (pixmap.width() as u32, pixmap.height() as u32);
    let rgba = unpremultiply(pixmap.data_as_u8_slice());
    Ok(PdfPageImage {
        width: w,
        height: h,
        rgba,
    })
}

/// 预乘 RGBA8 → 直通 RGBA8(gpui RenderImage 输入形态)
fn unpremultiply(premul: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(premul.len());
    for px in premul.as_chunks::<4>().0 {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        match a {
            0 => out.extend_from_slice(&[0, 0, 0, 0]),
            255 => out.extend_from_slice(&[r, g, b, a]),
            _ => {
                let un = |c: u8| ((c as u32 * 255 + (a as u32 / 2)) / a as u32).min(255) as u8;
                out.extend_from_slice(&[un(r), un(g), un(b), a]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手造最小合法 PDF(单页 MediaBox 200×100,整页黑矩形)。
    /// xref 偏移逐对象精确计算(解析器另有恢复兜底)
    fn minimal_pdf() -> Vec<u8> {
        let mut out = String::new();
        let mut offsets: Vec<usize> = Vec::new();
        out.push_str("%PDF-1.4\n");
        offsets.push(0); // 对象 1 起始偏移(占位,下方按实际记录)
        offsets.clear();
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R /Resources << >> >>",
        ];
        for (ix, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{}\nendobj\n", ix + 1, body));
        }
        let stream = "0 0 0 rg 10 10 180 80 re f";
        offsets.push(out.len());
        out.push_str(&format!(
            "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
            stream.len(),
            stream
        ));
        let xref_at = out.len();
        out.push_str("xref\n0 5\n0000000000 65535 f \n");
        for off in &offsets {
            out.push_str(&format!("{off:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n"
        ));
        out.into_bytes()
    }

    #[test]
    fn open_page_dims_parses_fixture() {
        let dims = open_page_dims(&minimal_pdf()).expect("解析应成功");
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0], (200., 100.));
    }

    #[test]
    fn render_page_rasterizes_black_rect_on_white() {
        let image = render_page(&minimal_pdf(), 0).expect("渲染应成功");
        assert_eq!(image.width, 1000, "目标宽 = RENDER_WIDTH");
        assert_eq!(image.height, 500, "等比 200:100");
        assert_eq!(image.rgba.len(), (1000 * 500 * 4) as usize);
        // 中心 = 黑矩形;角 = 白底(直通化后 alpha 恒 255)
        let px = |x: u32, y: u32| {
            let i = ((y * image.width + x) * 4) as usize;
            (
                image.rgba[i],
                image.rgba[i + 1],
                image.rgba[i + 2],
                image.rgba[i + 3],
            )
        };
        let (r, g, b, a) = px(500, 250);
        assert_eq!((r, g, b, a), (0, 0, 0, 255), "页心应为黑");
        let (r, g, b, a) = px(2, 2);
        assert_eq!((r, g, b, a), (255, 255, 255, 255), "页角应为白底");
    }

    #[test]
    fn garbage_bytes_map_to_invalid() {
        let err = open_page_dims(b"not a pdf at all");
        assert!(matches!(err, Err(PdfError::Invalid(_))));
    }

    #[test]
    fn unpremultiply_roundtrip() {
        assert_eq!(unpremultiply(&[10, 20, 30, 0]), vec![0, 0, 0, 0]);
        assert_eq!(unpremultiply(&[10, 20, 30, 255]), vec![10, 20, 30, 255]);
        // 半透红预乘 → 直通近似还原
        let out = unpremultiply(&[128, 0, 0, 128]);
        assert_eq!((out[0], out[3]), (255, 128));
    }
}
