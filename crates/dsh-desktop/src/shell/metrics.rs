//! 对话列宽策略(全应用统一):恒定默认宽度 MIN_COL=748,窄窗让位
//! (扣左锚点槽/右滚动条槽,不足吃满可用宽)。
//!
//! 消息列 / composer / plan_review / todo_dock / hero 共用同一宽度、
//! 同一中心线,不随窗口宽度伸缩(用户拍板:composer 恒定默认宽度,
//! 不压缩——此前的 50%/780 过渡带已废)。侧栏宽常量也收口于此
//! (展开 280 / 让位隐藏 = 0,见 sync_sidebar_yield)。

use gpui_kit::{Pixels, Window, px};

/// 侧栏展开默认宽(无用户拖宽前)
pub const SIDEBAR_W: f32 = 280.;
/// 侧栏拖动 clamp 下限(协议冻结几何,264)
pub const SIDEBAR_MIN: f32 = 264.;
/// 侧栏拖动 clamp 上限(协议冻结几何,420)
pub const SIDEBAR_MAX: f32 = 420.;
/// 右侧面板默认宽 = 下限(开面板即最小宽,存储宽不存在低于下限的
/// 形态,消除「窄开→首拖弹跳」;原 380)
pub const PANEL_W: f32 = PANEL_MIN;
/// 右侧面板拖宽下限(320→640 实测过宽,终值 540)
pub const PANEL_MIN: f32 = 540.;
/// 运行时长时钟阈值(<15s 只出纯标签,不闪时钟)
pub const RUN_CLOCK_AFTER_SECS: u64 = 15;
/// 列与内容区两侧的水平留白(滚动区/底部栈各 16px padding)
pub const H_PAD: f32 = 16.;
/// 左缘锚点槽(防锚点侵入内容区):导航轨刻度线起点
/// 距 content-card 左缘 24、hover 激活最长 26 → 占位到 50;窄窗下列宽
/// 吃满内容区时,刻度会叠在文字上——内容列必须给它让位。取 56(与折
/// 叠 rail 同宽的视觉节奏)。
pub const NAV_GUTTER_W: f32 = 56.;
/// 右缘滚动条槽(防滚动条侵入内容区):组件库 Scrollbar 轨道宽
/// 16(THUMB_INSET 4),留 24 呼吸。滚动条挂 content-card 右缘不动,
/// 让位靠列宽扣减。
pub const SCROLLBAR_GUTTER_W: f32 = 24.;
/// 列宽(恒定默认宽度,不随窗口伸缩)
pub const MIN_COL: f32 = 748.;
/// 用户气泡占列宽比例(原 525/748 ≈ 70%,随列等比)
pub const BUBBLE_RATIO: f32 = 0.7;
/// 面板让位下限:面板打开时渲染宽低于此 → 整体让位隐藏(避免细条;
/// 窗口变宽自动恢复,见 AppStore::sync_yield_negotiation)
pub const PANEL_YIELD_MIN: f32 = 320.;
/// 聊天内容区需求宽 = 对话列默认宽(MIN_COL)+ 双槽 + 两侧留白。
/// 面板让位/拖宽协商预留此宽(面板打开时对话列仍恒 748);侧栏
/// 让位同一阈值(窗口减侧栏不足此宽 → 左侧栏自动隐藏,
/// 见 AppStore::sync_sidebar_yield);窗口硬下限由 window_min_size
/// (960 ≥ 此值)兜底
pub const CHAT_AREA_MIN: f32 = MIN_COL + 2. * H_PAD + NAV_GUTTER_W + SCROLLBAR_GUTTER_W;

/// 侧栏拖宽 clamp 到协议范围 [SIDEBAR_MIN, SIDEBAR_MAX]。
pub fn clamp_sidebar(px: f32) -> f32 {
    px.clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

/// 面板拖宽上限(无固定常量):视口 − 侧栏(当前形态)− 聊天最小宽,
/// PANEL_MIN 托底(仅约束拖宽协商;渲染宽由 panel_width_for 真让位)。
/// 侧栏展开态算出更小的上限,收起态(隐藏)算出更宽的上限——正好对应
/// 「先压聊天区 → 收左栏 → 继续压」协商序。
pub fn panel_limit(viewport_w: f32, sidebar_collapsed: bool, sidebar_px: f32) -> f32 {
    (viewport_w - f32::from(sidebar_width_for(sidebar_collapsed, sidebar_px)) - CHAT_AREA_MIN)
        .max(PANEL_MIN)
}

/// 面板渲染宽(仅 open 时占宽;收起 = 0)。**真让位**:窗口窄到侧栏
/// 之外放不下「MIN_COL + 意愿宽」时,面板就地连续压窄(下限 0 =
/// 不渲染),聊天列拿回 MIN_COL——此前渲染宽被 PANEL_MIN 托底钉死
/// 540,最窄窗(960)下面板 flex_shrink_0 不缩,内容区被压破(导航
/// 轨/滚动条双双漂进内容区)。PANEL_MIN 只约束
/// 拖宽协商(panel_limit),不托底渲染宽;拖宽改动的 panel_px 在窄窗
/// 下被让位公式压住,渲染宽不跳变,窗口拉宽后自然回到意愿宽。
pub fn panel_width_for(
    open: bool,
    width: f32,
    viewport_w: f32,
    sidebar_collapsed: bool,
    sidebar_px: f32,
) -> Pixels {
    px(if open {
        let sidebar = f32::from(sidebar_width_for(sidebar_collapsed, sidebar_px));
        width.min((viewport_w - sidebar - CHAT_AREA_MIN).max(0.))
    } else {
        0.
    })
}

/// 侧栏宽(展开用存储的 `sidebar_px` 拖宽值,收起完全隐藏 = 0;
/// 折叠不再保留 56px rail)。
/// `width` 仅展开态生效;收起恒返 0。
pub fn sidebar_width_for(collapsed: bool, width: f32) -> Pixels {
    px(if collapsed { 0. } else { clamp_sidebar(width) })
}

/// 对话列宽:恒定默认宽度 MIN_COL;窄窗可用宽(扣左右边槽 56+24)
/// 不足时吃满可用宽(锚点与滚动条 thumb 不叠文字)。
/// content_w = 窗口宽 − 侧栏宽。
pub fn chat_col_w(content_w: Pixels) -> Pixels {
    let avail = content_w - px(2. * H_PAD + NAV_GUTTER_W + SCROLLBAR_GUTTER_W);
    px(MIN_COL).min(avail).max(px(0.))
}

/// 由窗口直接推对话列宽(内容区 = viewport − 侧栏 − 右面板;侧栏宽取
/// 展开态拖宽值,收起态由 `sidebar_width_for` 归一为 0;面板仅打开
/// 时占宽)
pub fn window_chat_col_w(
    window: &Window,
    sidebar_collapsed: bool,
    sidebar_px: f32,
    panel_open: bool,
    panel_px: f32,
) -> Pixels {
    chat_col_w(
        window.viewport_size().width
            - sidebar_width_for(sidebar_collapsed, sidebar_px)
            - panel_width_for(
                panel_open,
                panel_px,
                f32::from(window.viewport_size().width),
                sidebar_collapsed,
                sidebar_px,
            ),
    )
}

/// 用户气泡宽(列宽 70%)
pub fn bubble_w(col_w: Pixels) -> Pixels {
    col_w * BUBBLE_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn col_width_policy() {
        // 恒定默认宽度:中小窗 748,大屏也 748(不随窗口伸缩)
        assert_eq!(chat_col_w(px(1160.)), px(748.)); // 1440 默认窗 − 280
        assert_eq!(chat_col_w(px(2280.)), px(748.)); // 2560 窗 − 280
        assert_eq!(chat_col_w(px(1560.)), px(748.));
        // 窄窗:可用宽(扣左右边槽 56+24)本身不足 748 → 填满可用宽
        assert_eq!(
            chat_col_w(px(744.)),
            px(744. - 32. - NAV_GUTTER_W - SCROLLBAR_GUTTER_W)
        );
    }

    #[test]
    fn bubble_ratio_preserves_legacy_proportion() {
        // 原 525/748 ≈ 70.2%:748 列下气泡 ≈ 523.6,视觉不变
        let bw = bubble_w(px(748.));
        let d = bw - px(523.6);
        assert!(d > px(-0.5) && d < px(0.5), "bubble {bw:?}");
        // 随列等比放宽
        let bw = bubble_w(px(1140.));
        let d = bw - px(798.);
        assert!(d > px(-0.5) && d < px(0.5), "bubble {bw:?}");
    }

    #[test]
    fn sidebar_clamp_range() {
        // 源 clampWidth(SIDEBAR_MIN..SIDEBAR_MAX):默认 280 在范围内不截断
        assert_eq!(clamp_sidebar(280.), 280.);
        // 下界
        assert_eq!(clamp_sidebar(100.), SIDEBAR_MIN);
        // 上界
        assert_eq!(clamp_sidebar(900.), SIDEBAR_MAX);
        // 边界值原样保留
        assert_eq!(clamp_sidebar(SIDEBAR_MIN), SIDEBAR_MIN);
        assert_eq!(clamp_sidebar(SIDEBAR_MAX), SIDEBAR_MAX);
    }

    #[test]
    fn panel_limit_negotiates_with_sidebar_form() {
        // 展开态上限 = 1600 − 280 − 860 = 460(低于下限 540,触底)
        assert_eq!(panel_limit(1600., false, SIDEBAR_W), PANEL_MIN);
        // 收起态(完全隐藏)= 1600 − 0 − 860 = 740
        assert_eq!(panel_limit(1600., true, SIDEBAR_W), 740.);
        // 极窄窗:上限触底 PANEL_MIN(面板仍在,聊天区让位)
        assert_eq!(panel_limit(800., false, SIDEBAR_W), PANEL_MIN);
        // 渲染兜底:存储宽超上限就地让位;面板收起恒 0
        assert_eq!(
            panel_width_for(true, 900., 1600., false, SIDEBAR_W),
            px(460.)
        );
        assert_eq!(
            panel_width_for(true, 400., 1600., false, SIDEBAR_W),
            px(400.)
        );
        assert_eq!(
            panel_width_for(false, 400., 1600., false, SIDEBAR_W),
            px(0.)
        );
    }
}
