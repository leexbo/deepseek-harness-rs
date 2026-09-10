//! 全高轨道滚动条 handle(修「滚动条不能拉到底部」)。
//!
//! 组件库 `Scrollbar` 的 thumb 映射把轨道自身高度(hitbox 高)当作滚动
//! 视口:轨道挂 content-card 全高时,列表视口之外的差值段(底部栈/
//! 状态栏)成为盲区——拖到轨道底,列表还差一截滚不到底。
//! 修法:包装 [`gpui_kit::ListState`],content_size 报告值加上 extra =
//! (轨道高 − 列表视口高),使 `scroll_area − track == content − viewport`,
//! thumb 的满行程精确映射到列表的真实滚动域;轨道高由渲染期 canvas
//! 捕获存 store,视口高实时读 ListState。

use gpui_kit::component::scroll::ScrollbarHandle;
use gpui_kit::{Bounds, ListState, Pixels, Point, Size, px};

/// 全高轨道 handle:滚动委托 ListState,content_size 加 extra 补偿。
#[derive(Clone)]
pub struct FullTrackHandle {
    list: ListState,
    /// 轨道高 − 列表视口高(渲染期捕获,上一帧值;布局稳定后收敛)
    extra: Pixels,
}

impl FullTrackHandle {
    pub fn new(list: &ListState, extra: Pixels) -> Self {
        Self {
            list: list.clone(),
            extra: extra.max(px(0.)),
        }
    }
}

impl ScrollbarHandle for FullTrackHandle {
    fn offset(&self) -> Point<Pixels> {
        self.list.scroll_px_offset_for_scrollbar()
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.list.set_offset_from_scrollbar(offset);
    }

    fn content_size(&self) -> Size<Pixels> {
        let content =
            self.list.viewport_bounds().size + Size::from(self.list.max_offset_for_scrollbar());
        content + Size::new(px(0.), self.extra)
    }

    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
    }
}
