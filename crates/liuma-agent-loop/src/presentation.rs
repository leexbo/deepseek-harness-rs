//! 工具渲染意图(render intent):类型化视图词汇表。
//!
//! 分工三原则(三层机制):
//! 1. **工具侧声明**:工具在文本扁平化**之前**持有真值(行窗口/分组/
//!    截断标志/退出状态),在此构造 [`ToolView`]——call 侧经
//!    [`ToolPort::present_call`](crate::ToolPort::present_call)(运行中
//!    意图,如 file_edit 的 old/new diff),result 侧随
//!    [`ToolOutput::view`](crate::ToolOutput::view)。
//! 2. **持久化与回放**:视图随 tool/call、tool/result 事件落档,历史
//!    回放结构化无损——旧会话渲染与新会话恒等。
//! 3. **UI 侧通用渲染**:UI 对展开体 switch `card`,从不 switch 工具名;
//!    非法/未知视图窄化为 None → 通用 IN/OUT 卡(`narrowDiffs`
//!    的防御姿态)。
//!
//! wire 形状不追求逐字节复刻(`card`+`shape` 二级判别),
//! serde 内部标签 enum 直接序列化即可——消费端(桌面)只认自有窄化类型。

use serde::{Deserialize, Serialize};

/// 一行带文件行号的内容(read/search 视图共用的最小单元)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewLine {
    /// 1 起文件行号(offset 窗口保留文件自身编号,不重新从 1 计)
    pub number: u64,
    /// 行文本(无尾随换行,已含工具的逐行截断)
    pub text: String,
}

/// 一个文件的分组内容匹配(search matches 形态)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMatches {
    /// 文件路径(模型面显示路径)
    pub path: String,
    /// 该文件的匹配行(输出序)
    pub matches: Vec<ViewLine>,
}

/// 单文件变更(意图或事实)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    /// 文件路径
    pub path: String,
    /// 先前内容;None = 新建/覆写(调用侧无先前内容可用)
    pub old_text: Option<String>,
    /// 变更后内容
    pub new_text: String,
}

/// 工具渲染意图:判别联合,`card` 为标签。
///
/// output 文本不进视图:事件顶层 `output` 字段是**模型派生消费面**
/// (日志派生 → 出网请求),终端/读取卡的正文直接复用它——视图只携带
/// 文本扁平化不可逆的结构字段(行号窗口/总数/分组/截断/退出状态)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "card", rename_all = "camelCase")]
pub enum ToolView {
    /// 终端命令执行(bash 前台/PTY 的 result 态)
    Terminal {
        /// 退出码(落定即数据:非零退出不是执行失败)
        #[serde(rename = "exitCode")]
        exit_code: Option<i32>,
        /// 终止信号名(如 SIGTERM;与 exit_code 互斥)
        signal: Option<String>,
        /// 工作目录(提示符标签)
        cwd: Option<String>,
    },
    /// 文件读窗口(file_read 的 result 态)
    Read {
        /// 文件路径(模型面;UI 渲染时工作区相对化)
        path: String,
        /// 窗口首行(1 起;空窗口也保留,续读从此处恢复)
        offset: u64,
        /// 窗口行(文件序,保留文件自身行号)
        lines: Vec<ViewLine>,
        /// 文件总行数(「显示 N / M 行」数据源)
        #[serde(rename = "totalLines")]
        total_lines: u64,
        /// 语法高亮语言提示(扩展名映射;None = 未知,纯等宽文本)
        lang: Option<String>,
    },
    /// 内容检索(file_search content 形态的 result 态):匹配按文件分组
    SearchMatches {
        /// 分组(首见文件序)
        files: Vec<FileMatches>,
        /// 结果是否被上限截断(files 只含保留部分)
        truncated: bool,
        /// 截断前总匹配数(未截断时 = 保留数)
        total: u64,
    },
    /// 路径检索(file_search glob-only 形态的 result 态)
    SearchPaths {
        /// 路径列表(工具结果序;截断时为保留页)
        paths: Vec<String>,
        /// 结果是否被上限截断
        truncated: bool,
        /// 截断前总路径数(未截断时 = 路径数)
        total: u64,
    },
    /// 文件变更 diff(call 侧 = 运行中意图,自 args 构造;
    /// result 侧 = 已应用事实)。编辑失败(Error)无 diff 视图
    Diff {
        /// 每文件一条,文件序
        diffs: Vec<FileDiff>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 形状:`card` 内部标签 + camelCase 字段(桌面窄化的输入面)
    #[test]
    fn wire_shape_is_card_tagged_camel_case() {
        let v = ToolView::Read {
            path: "a.rs".into(),
            offset: 3,
            lines: vec![ViewLine {
                number: 3,
                text: "fn main()".into(),
            }],
            total_lines: 10,
            lang: Some("rust".into()),
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["card"], "read");
        assert_eq!(json["path"], "a.rs");
        assert_eq!(json["totalLines"], 10);
        assert_eq!(json["lines"][0]["number"], 3);
        // 往返无损(持久化与回放恒等的基础)
        assert_eq!(serde_json::from_value::<ToolView>(json).unwrap(), v);
    }

    #[test]
    fn terminal_and_diff_wire_shapes() {
        let v = ToolView::Terminal {
            exit_code: Some(1),
            signal: None,
            cwd: Some("/w/proj".into()),
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["card"], "terminal");
        assert_eq!(json["exitCode"], 1);

        let d = ToolView::Diff {
            diffs: vec![FileDiff {
                path: "a.txt".into(),
                old_text: None,
                new_text: "new".into(),
            }],
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["card"], "diff");
        assert_eq!(json["diffs"][0]["oldText"], serde_json::Value::Null);
    }
}
