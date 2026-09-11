//! merge:逐点合并为最严格结果(源 hook-protocol/merge.ts 逐字对齐)。
//!
//! rank:deny/block=3 > ask=2 > approve/allow=1 > 无=0;reason 只从
//! 获胜 rank 收集、`\n\n` 连接;stop 粘滞取第一个 continue:false 的
//! stopReason;additionalContext / systemMessages 按钩子顺序累积、
//! 跳过空串;空输入 = 中性结果。

use crate::codec::{Decision, HookOutput};

/// 合并后的单点决策(源 MergedDecision;`None` = 无钩子表态)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergedDecision {
    Allow,
    Ask,
    Deny,
    #[default]
    None,
}

/// 单点折叠结果(源 MergedHookOutcome)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergedHookOutcome {
    pub decision: MergedDecision,
    /// 获胜 rank 的 blocking/denying 理由(`\n\n` 连接;None = 无)
    pub reason: Option<String>,
    /// 任一钩子 continue:false
    pub stop: bool,
    /// 第一个 halting 钩子的 stopReason
    pub stop_reason: Option<String>,
    /// 每个钩子的 additionalContext(钩子序,不连接——桥决定)
    pub additional_context: Vec<String>,
    /// 每个钩子的 systemMessage(钩子序)
    pub system_messages: Vec<String>,
}

/// 决策排名(高 = 更严)
fn rank(decision: Option<Decision>) -> u8 {
    match decision {
        Some(Decision::Deny) | Some(Decision::Block) => 3,
        Some(Decision::Ask) => 2,
        Some(Decision::Allow) | Some(Decision::Approve) => 1,
        None => 0,
    }
}

fn decision_for_rank(max: u8) -> MergedDecision {
    match max {
        3 => MergedDecision::Deny,
        2 => MergedDecision::Ask,
        1 => MergedDecision::Allow,
        _ => MergedDecision::None,
    }
}

/// 折叠一次 hook 点全部命中钩子的输出(源 mergeHookOutputs;顺序无关
/// 于决策,上下文保序)。空列表 = 中性结果。
pub fn merge_hook_outputs(outputs: &[HookOutput]) -> MergedHookOutcome {
    let mut max_rank = 0u8;
    // reason 按 rank 分桶:只有获胜决策的理由浮出
    let mut reasons_by_rank: std::collections::HashMap<u8, Vec<String>> = Default::default();
    let mut stop = false;
    let mut stop_reason: Option<String> = None;
    let mut additional_context = Vec::new();
    let mut system_messages = Vec::new();

    for out in outputs {
        let r = rank(out.decision);
        if r > max_rank {
            max_rank = r;
        }
        if (r == 3 || r == 2)
            && let Some(reason) = &out.reason
            && !reason.is_empty()
        {
            reasons_by_rank.entry(r).or_default().push(reason.clone());
        }
        if out.continue_ == Some(false) && !stop {
            stop = true;
            stop_reason = out.stop_reason.clone();
        }
        if let Some(c) = &out.additional_context
            && !c.is_empty()
        {
            additional_context.push(c.clone());
        }
        if let Some(m) = &out.system_message
            && !m.is_empty()
        {
            system_messages.push(m.clone());
        }
    }

    let reasons = reasons_by_rank.get(&max_rank).cloned().unwrap_or_default();
    MergedHookOutcome {
        decision: decision_for_rank(max_rank),
        reason: if reasons.is_empty() {
            None
        } else {
            Some(reasons.join("\n\n"))
        },
        stop,
        stop_reason,
        additional_context,
        system_messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(decision: Option<Decision>, reason: Option<&str>) -> HookOutput {
        HookOutput {
            decision,
            reason: reason.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn empty_is_neutral() {
        let m = merge_hook_outputs(&[]);
        assert_eq!(m.decision, MergedDecision::None);
        assert!(!m.stop);
        assert!(m.additional_context.is_empty());
    }

    #[test]
    fn precedence_order_independent_block_folds_to_deny() {
        let outputs = [
            out(Some(Decision::Allow), None),
            out(Some(Decision::Ask), Some("why ask")),
            out(Some(Decision::Block), Some("no")),
        ];
        let m = merge_hook_outputs(&outputs);
        assert_eq!(m.decision, MergedDecision::Deny);
        assert_eq!(m.reason.as_deref(), Some("no"));
        // 逆序同结果(fold 与顺序无关)
        let mut rev = outputs;
        rev.reverse();
        assert_eq!(merge_hook_outputs(&rev), m);
    }

    #[test]
    fn reasons_only_from_winning_rank_joined_blank_line() {
        let outputs = [
            out(Some(Decision::Ask), Some("ask reason")),
            out(Some(Decision::Deny), Some("deny a")),
            out(Some(Decision::Deny), Some("deny b")),
        ];
        let m = merge_hook_outputs(&outputs);
        assert_eq!(m.reason.as_deref(), Some("deny a\n\ndeny b"));
        // ask 胜出时显示 ask 理由
        let m = merge_hook_outputs(&[out(Some(Decision::Ask), Some("why"))]);
        assert_eq!(m.reason.as_deref(), Some("why"));
        // allow 的理由不收集
        let m = merge_hook_outputs(&[out(Some(Decision::Allow), Some("meh"))]);
        assert_eq!(m.reason, None);
    }

    #[test]
    fn stop_sticky_first_reason() {
        let mut a = out(None, None);
        a.continue_ = Some(false);
        a.stop_reason = Some("first".into());
        let mut b = out(None, None);
        b.continue_ = Some(false);
        b.stop_reason = Some("second".into());
        let m = merge_hook_outputs(&[a.clone(), b]);
        assert!(m.stop);
        assert_eq!(m.stop_reason.as_deref(), Some("first"));
        // 无 reason 也 stop
        let m = merge_hook_outputs(&[out(None, None)]);
        assert!(!m.stop);
    }

    #[test]
    fn context_accumulates_in_order_skipping_empty() {
        let mut a = out(None, None);
        a.additional_context = Some("one".into());
        let mut b = out(None, None);
        b.additional_context = Some(String::new());
        let mut c = out(None, None);
        c.additional_context = Some("two".into());
        c.system_message = Some("warn".into());
        let m = merge_hook_outputs(&[a, b, c]);
        assert_eq!(
            m.additional_context,
            vec!["one".to_string(), "two".to_string()]
        );
        assert_eq!(m.system_messages, vec!["warn".to_string()]);
    }
}
