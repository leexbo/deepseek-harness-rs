//! 请求重试策略:有界指数退避 + 对称抖动
//! (normal 模式默认值:5 次 / 500ms 起 / ×2 指数 / 封顶 10s / ±10% 抖动)。
//!
//! 决策入口是 [`RetryPolicy::decide`]:按 [`TransportError`] 分类与
//! 第 retry 次序号给出等待时长或放行。延迟计算为纯函数(`random`
//! 注入),节奏断言无需时钟基建;引擎持有随机源,测试可注入固定样本。

use std::time::Duration;

use crate::transport::TransportError;

/// 有界指数退避策略(默认 normal 模式:5 次 / 500ms 起 /
/// ×2 指数 / 封顶 10s / ±10% 抖动)
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// 首次请求后最多重试次数(重试编号 > max_retries 即放行错误)
    pub max_retries: u32,
    /// 退避基值(毫秒)
    pub initial_delay_ms: u64,
    /// 单次延迟上限(毫秒;本地退避与服务端 Retry-After 同限)
    pub max_delay_ms: u64,
    /// 对称抖动比例(0.1 → 实际延迟 ∈ [0.9, 1.1]× 指数值)
    pub jitter_ratio: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_delay_ms: 500,
            max_delay_ms: 10_000,
            jitter_ratio: 0.1,
        }
    }
}

impl RetryPolicy {
    /// 第 `retry` 次重试的本地退避延迟
    /// (initial × 2^(retry-1) 先封顶,乘 [1-j, 1+j] 对称抖动后再封顶)。
    /// `random` ∈ [0,1] 由调用方注入(测试传固定样本;运行时为引擎随机源)。
    pub fn local_delay_ms(&self, retry: u32, random: f64) -> u64 {
        let exponent = retry.saturating_sub(1).min(1024);
        let exponential =
            (self.initial_delay_ms as f64 * 2.0f64.powi(exponent as i32)).min(self.max_delay_ms as f64);
        let jitter = 1.0 - self.jitter_ratio + 2.0 * self.jitter_ratio * random.clamp(0.0, 1.0);
        let ms = (exponential * jitter).min(self.max_delay_ms as f64);
        // 退避等待至少 1ms:0 会退化为忙循环式的立即重发
        (ms.round() as u64).max(1)
    }

    /// 一次失败的重试决策:`Some(delay)` = 重试并先等待 delay;
    /// `None` = 放行错误(不可重试分类 / 已耗尽 / 服务端 Retry-After
    /// 超上限)。`retry` 从 1 计(第一次失败 → 第 1 次重试)。
    ///
    /// 服务端 Retry-After 在上限内**直接采用,不加抖动**;
    /// 超上限按放行处理(normal 模式语义:无界等待不属既定策略)。
    pub fn decide(&self, failure: &TransportError, retry: u32, random: f64) -> Option<Duration> {
        if !failure.retryable() {
            return None;
        }
        if retry > self.max_retries {
            return None;
        }
        let delay_ms = match failure.retry_after_ms() {
            Some(ra) if ra > self.max_delay_ms => return None,
            Some(ra) => ra,
            None => self.local_delay_ms(retry, random),
        };
        Some(Duration::from_millis(delay_ms.max(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    /// 退避序列(抖动中点):500 → 1000 → 2000 → 4000 → 8000
    #[test]
    fn backoff_sequence_doubles() {
        let p = policy();
        let seq: Vec<u64> = (1..=5).map(|r| p.local_delay_ms(r, 0.5)).collect();
        assert_eq!(seq, vec![500, 1000, 2000, 4000, 8000]);
    }

    /// 指数延迟封顶 10s(±10% 抖动后仍不越界)
    #[test]
    fn exponential_capped_at_max_delay() {
        let p = policy();
        // retry 6 本应 16s,封顶 10s;抖动上界 1.1× 亦封在 10s
        assert_eq!(p.local_delay_ms(6, 0.5), 10_000);
        assert_eq!(p.local_delay_ms(6, 1.0), 10_000);
        // 深位重试(超大指数)不溢出
        assert_eq!(p.local_delay_ms(2000, 0.5), 10_000);
    }

    /// 抖动界:[1-j, 1+j] × 指数值(random=0 下界、1 上界)
    #[test]
    fn jitter_bounds_symmetric() {
        let p = policy();
        assert_eq!(p.local_delay_ms(1, 0.0), 450); // 500 × 0.9
        assert_eq!(p.local_delay_ms(1, 1.0), 550); // 500 × 1.1
        assert_eq!(p.local_delay_ms(2, 0.0), 900);
        assert_eq!(p.local_delay_ms(2, 1.0), 1100);
    }

    /// 可重试分类放行,不可重试直通
    #[test]
    fn decide_by_classification() {
        let p = policy();
        assert!(p.decide(&TransportError::Transport("conn".into()), 1, 0.5).is_some());
        assert!(p.decide(&TransportError::Timeout("t".into()), 1, 0.5).is_some());
        assert!(p
            .decide(
                &TransportError::Server { status: 502, body: "bad gw".into() },
                1,
                0.5
            )
            .is_some());
        assert!(p
            .decide(
                &TransportError::RateLimit { retry_after_ms: None, body: "slow".into() },
                1,
                0.5
            )
            .is_some());
        assert!(p.decide(&TransportError::EmptyResponse, 1, 0.5).is_some());
        // 直通:401/400 与未分类
        assert!(p
            .decide(
                &TransportError::Auth { status: 401, body: "nope".into() },
                1,
                0.5
            )
            .is_none());
        assert!(p
            .decide(
                &TransportError::InvalidRequest { status: 400, body: "bad".into() },
                1,
                0.5
            )
            .is_none());
        assert!(p.decide(&TransportError::Other("mystery".into()), 1, 0.5).is_none());
    }

    /// 耗尽:第 max_retries 次仍放行重试,其后放行错误
    #[test]
    fn decide_exhausts_after_max_retries() {
        let p = policy();
        assert!(p.decide(&TransportError::EmptyResponse, 5, 0.5).is_some());
        assert!(p.decide(&TransportError::EmptyResponse, 6, 0.5).is_none());
    }

    /// 服务端 Retry-After:上限内原样采用(无抖动),超上限放行
    #[test]
    fn retry_after_adopted_verbatim_within_cap() {
        let p = policy();
        let f = TransportError::RateLimit { retry_after_ms: Some(7_777), body: String::new() };
        assert_eq!(p.decide(&f, 1, 0.0), Some(Duration::from_millis(7_777)));
        let over = TransportError::RateLimit { retry_after_ms: Some(60_000), body: String::new() };
        assert_eq!(p.decide(&over, 1, 0.5), None);
    }
}
