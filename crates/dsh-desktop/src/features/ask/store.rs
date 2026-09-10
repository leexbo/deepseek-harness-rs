//! 问答/计划审批的 store 域:UI 交互态(AskUiState)+ 应答/取消/分页/
//! 提交/自定义输入。pending_ask/pending_plan 是 StoreState 的会话镜像
//! (reducer 侧),本域应答后经 host.respond 回填。

use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::{AppContext, Context, Entity, Window};

use dsh_core::proto::RpcResult;

use crate::shell::store::AppStore;

/// 问答卡 UI 交互态(每题选中 labels;index 当前题)
#[derive(Debug, Clone)]
pub(crate) struct AskUiState {
    /// 当前题 index(pager)
    pub index: usize,
    /// 每题选中的 option labels(单/多选;并行下标与问题对齐)
    pub selected: Vec<Vec<String>>,
    /// 每题自定义文本(并行下标)
    pub custom: Vec<String>,
}

/// 问答/计划审批功能切片状态(问答卡交互态 + 「其他」输入)。
#[derive(Default)]
pub(crate) struct AskStore {
    /// 问答卡交互态(当前题 index + 每题选中 labels + 自定义文本)
    pub ask_state: Option<AskUiState>,
    /// 问答卡「其他」输入(自定义文本;挂窗后经 ensure_ask_input 懒建,
    /// render 期读值渲染,Change 订阅写回 ask_state.custom[当前题])
    pub ask_input: Option<Entity<TextareaState>>,
    /// 审批卡选项②「否,并告诉它应该如何做不同」的行内输入(直接在
    /// 卡内输入,非拒绝后聚焦 composer;ensure 懒建同上)
    pub plan_decline_input: Option<Entity<TextareaState>>,
    /// 审批卡选项选择态(选择→批准两步;None=未选,
    /// Some(true)=①实施 / Some(false)=②修改意见)。提交后随 pending 清空
    pub plan_selection: Option<bool>,
}

impl AppStore {
    /// 计划审批应答(approve = 批准标签;应答形状见 registry respond)
    pub fn answer_plan(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Some(plan) = self.state.pending_plan.take() else {
            return;
        };
        self.ask.plan_decline_input = None;
        self.ask.plan_selection = None;
        // 应答 label 优先用发起方 options(label 随答案回给模型)
        let opts = plan.question.options.clone().unwrap_or_default();
        let label = if approve {
            opts.first()
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "批准".into())
        } else {
            opts.get(1)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "拒绝".into())
        };
        let result = RpcResult::Ok(serde_json::json!({
            "sessionId": plan.session_id,
            "answer": { "answers": [ { "id": plan.question.id, "selected": [label] } ] },
        }));
        self.bridge.host().respond(&plan.rpc_id, &result);
        cx.notify();
    }

    /// 去聊天里说(源 PlanReviewPanel 第三动作):取消请求 → 工具收到
    /// 取消结果,模型回到对话;composer 恢复,用户直接说修改意见
    pub fn dismiss_plan(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = self.state.pending_plan.take() else {
            return;
        };
        self.ask.plan_decline_input = None;
        self.ask.plan_selection = None;
        self.bridge.host().respond(
            &plan.rpc_id,
            &RpcResult::Err(dsh_core::proto::RpcError {
                code: "cancelled".into(),
                message: "用户取消,回到对话".into(),
                details: serde_json::Value::Null,
            }),
        );
        cx.notify();
    }

    /// 选项②「否,并告诉它应该如何做不同」提交
    /// (卡内行内输入,Enter 提交):拒绝应答 + custom 反馈,宿主经引导轮
    /// 直送模型(见 registry plan_decline_guide)
    pub fn decline_plan_with_feedback(&mut self, feedback: String, cx: &mut Context<Self>) {
        let Some(plan) = self.state.pending_plan.take() else {
            return;
        };
        self.ask.plan_decline_input = None;
        let opts = plan.question.options.clone().unwrap_or_default();
        let label = opts
            .get(1)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| "拒绝".into());
        let result = RpcResult::Ok(serde_json::json!({
            "sessionId": plan.session_id,
            "answer": { "answers": [ {
                "id": plan.question.id,
                "selected": [label],
                "custom": feedback,
            } ] },
        }));
        self.bridge.host().respond(&plan.rpc_id, &result);
        cx.notify();
    }

    /// 审批卡选项选择(选择→批准两步——点选项行
    /// 只标记选择,不提交;提交走「批准」钮)
    pub fn select_plan_option(&mut self, approve: bool, cx: &mut Context<Self>) {
        self.ask.plan_selection = Some(approve);
        cx.notify();
    }

    /// 提交所选选项:① → 批准;② → 有反馈 = 拒绝+反馈,空反馈 =
    /// 仅拒绝(原「跳过」语义并入)。未选择时为 no-op
    pub fn submit_plan_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.ask.plan_selection else {
            return;
        };
        if selected {
            self.answer_plan(true, cx);
            return;
        }
        let feedback = self
            .ask
            .plan_decline_input
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if feedback.is_empty() {
            self.answer_plan(false, cx);
        } else {
            self.decline_plan_with_feedback(feedback, cx);
        }
    }
    /// 懒建审批卡选项②行内输入(需要 Window;render 期首现调用——
    /// 仅选项②选中时渲染)。Enter(非 shift)= 按当前选择提交
    pub fn ensure_plan_decline_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ask.plan_decline_input.is_some() {
            return;
        }
        let input =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("否,并告诉它应该如何做不同"));
        cx.subscribe(&input, |this, _input, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { shift: false, .. } => {
                this.submit_plan_selection(cx);
            }
            InputEvent::Change => cx.notify(),
            _ => {}
        })
        .detach();
        self.ask.plan_decline_input = Some(input);
    }

    /// 通用问答应答(answers 数组;每项 {id, selected[], custom?})。
    /// 经 host.respond 回填,工具结果作为同一 tool-call 的 tool/result。
    pub fn answer_ask(&mut self, answers: Vec<serde_json::Value>, cx: &mut Context<Self>) {
        let Some(ask) = self.state.pending_ask.take() else {
            return;
        };
        let result = RpcResult::Ok(serde_json::json!({
            "sessionId": ask.session_id,
            "answer": { "answers": answers },
        }));
        self.bridge.host().respond(&ask.rpc_id, &result);
        cx.notify();
    }

    /// 沙箱升级审批(allow-once / rejected;应答形状见 registry respond
    /// Approval 分支 value["answer"]["approved"])
    pub fn answer_approval(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Some(p) = self.state.pending_approval.take() else {
            return;
        };
        let result = RpcResult::Ok(serde_json::json!({
            "sessionId": p.session_id,
            "answer": { "approved": approve },
        }));
        self.bridge.host().respond(&p.rpc_id, &result);
        cx.notify();
    }

    /// 取消审批(✕ → cancelled;模型收到逐字取消文案,审计对收口)
    pub fn dismiss_approval(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.state.pending_approval.take() else {
            return;
        };
        self.bridge.host().respond(
            &p.rpc_id,
            &RpcResult::Err(dsh_core::proto::RpcError {
                code: "cancelled".into(),
                message: "用户取消,回到对话".into(),
                details: serde_json::Value::Null,
            }),
        );
        cx.notify();
    }

    /// 放弃整组问题(取消;respond ok:false → host reject)
    pub fn cancel_ask(&mut self, cx: &mut Context<Self>) {
        let Some(ask) = self.state.pending_ask.take() else {
            return;
        };
        self.bridge.host().respond(
            &ask.rpc_id,
            &RpcResult::Err(dsh_core::proto::RpcError {
                code: "cancelled".into(),
                message: "user cancelled the question".into(),
                details: serde_json::Value::Null,
            }),
        );
        cx.notify();
    }

    // ── 问答卡 UI 交互(选中/自定义/分页/提交)────────────────────

    /// 初始化问答卡状态(进入 pending_ask 时;依问题集建空选中)
    fn ensure_ask_state(&mut self) {
        if self.ask.ask_state.is_none()
            && let Some(ask) = &self.state.pending_ask
        {
            self.ask.ask_state = Some(AskUiState {
                index: 0,
                selected: vec![Vec::new(); ask.questions.len()],
                custom: vec![String::new(); ask.questions.len()],
            });
        }
    }

    /// toggle 单选/多选 option(label)
    pub fn toggle_ask_option(&mut self, label: &str, cx: &mut Context<Self>) {
        self.ensure_ask_state();
        let Some(state) = self.ask.ask_state.as_mut() else {
            return;
        };
        let Some(ask) = &self.state.pending_ask else {
            return;
        };
        let i = state.index;
        let multi = ask
            .questions
            .get(i)
            .map(|q| q.multi_select.unwrap_or(false))
            .unwrap_or(false);
        let cell = &mut state.selected[i];
        if multi {
            if let Some(pos) = cell.iter().position(|l| l == label) {
                cell.remove(pos);
            } else {
                cell.push(label.to_string());
            }
        } else {
            // 单选:替换 + 清「其他」(选项与自定义互斥,源 choose 语义)
            cell.clear();
            cell.push(label.to_string());
            if let Some(c) = state.custom.get_mut(i) {
                c.clear();
            }
        }
        cx.notify();
    }

    /// 设置当前题自定义文本(「其他」输入 Change 接线)。语义循源
    /// draftCustom:单选取清选中(自定义覆盖选项),多选保留已勾标签。
    pub fn set_ask_custom(&mut self, text: &str, cx: &mut Context<Self>) {
        self.ensure_ask_state();
        let multi = self
            .state
            .pending_ask
            .as_ref()
            .and_then(|a| {
                a.questions
                    .get(self.ask.ask_state.as_ref().map(|s| s.index).unwrap_or(0))
            })
            .map(|q| q.multi_select.unwrap_or(false))
            .unwrap_or(false);
        if let Some(state) = self.ask.ask_state.as_mut() {
            if !multi && !text.is_empty() {
                // 单选取清:自定义文本输入即视为「其他」答案
                state.selected[state.index].clear();
            }
            if let Some(c) = state.custom.get_mut(state.index) {
                *c = text.to_string();
            }
        }
        cx.notify();
    }

    /// 懒建问答卡「其他」输入(需要 Window;render 期首现调用)。
    /// 订阅 Change → set_ask_custom 写回当前题;提交/取消后置位延迟清空
    /// (渲染期 set_value,与 composer 清空同模式)。
    pub fn ensure_ask_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ask.ask_input.is_some() {
            return;
        }
        let input = cx.new(|cx| TextareaState::new(window, cx).placeholder("输入你的答案"));
        cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let value = _input.read(cx).value().to_string();
                this.set_ask_custom(&value, cx);
            }
        })
        .detach();
        self.ask.ask_input = Some(input);
    }

    /// pager 前后翻题
    pub fn set_ask_index(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.ensure_ask_state();
        if let Some(state) = self.ask.ask_state.as_mut()
            && let Some(ask) = &self.state.pending_ask
        {
            state.index = idx.min(ask.questions.len().saturating_sub(1));
        }
        cx.notify();
    }

    /// 跳过本题(下一题;最后一题则视为空答提交)
    #[allow(dead_code)]
    pub fn skip_ask(&mut self, cx: &mut Context<Self>) {
        self.ensure_ask_state();
        let total = self
            .state
            .pending_ask
            .as_ref()
            .map(|a| a.questions.len())
            .unwrap_or(0);
        let idx = self.ask.ask_state.as_ref().map(|s| s.index).unwrap_or(0);
        if idx + 1 < total {
            self.set_ask_index(idx + 1, cx);
        } else {
            // 最后一题:收集答案提交(跳过题为空 selected)
            self.submit_ask(cx);
        }
    }

    /// 提交整组(收集每题的 selected/custom;空答也提交——源跳过语义)
    pub fn submit_ask(&mut self, cx: &mut Context<Self>) {
        self.ensure_ask_state();
        let answers: Vec<serde_json::Value> = {
            let Some(ask) = &self.state.pending_ask else {
                return;
            };
            let state = self
                .ask
                .ask_state
                .as_ref()
                .cloned()
                .unwrap_or_else(|| AskUiState {
                    index: 0,
                    selected: vec![Vec::new(); ask.questions.len()],
                    custom: vec![String::new(); ask.questions.len()],
                });
            ask.questions
                .iter()
                .enumerate()
                .map(|(i, q)| {
                    let mut item = serde_json::Map::new();
                    item.insert("id".into(), serde_json::json!(q.id));
                    let selected = state.selected.get(i).cloned().unwrap_or_default();
                    item.insert("selected".into(), serde_json::json!(selected));
                    let custom = state.custom.get(i).cloned().unwrap_or_default();
                    if !custom.is_empty() {
                        item.insert("custom".into(), serde_json::json!(custom));
                    }
                    serde_json::Value::Object(item)
                })
                .collect()
        };
        self.ask.ask_state = None;
        self.answer_ask(answers, cx);
    }
}
