//! 状态机:三态判定的平台无关纯函数(最易单测,DESIGN §14.5)。
//!
//! v1 的大 `evaluate(...)` 函数因 v2 需要**跨评估周期**的 per-pid 流量状态(`PidFlow`,见
//! `monitor.rs`)而不再是纯函数,其逐 pid 聚合 + 判定循环已下沉到 Engine 内部。`state.rs`
//! 只保留三个纯判定函数,由 Engine 在循环里调用:
//! - `is_effective_activity`:本周期是否构成「有效活动」(下行流 或 上行请求突发)。
//! - `classify`:由存活连接数 + 距上次有效活动的时长判三态。
//! - `overall`:机器级聚合(沿用 v1)。

use crate::model::*;
use std::time::Duration;

/// 本周期是否构成「有效活动」(DESIGN §14.4 步骤 4)。
///
/// L7 语义逼近:TLS 加密下无法读应用层,只能用 L4 可见特征逼近「此刻有没有一次进行中的
/// 请求 / SSE 流式响应」。两个 OR 条件:
/// - `down_rate >= DOWN_RATE_ACTIVE_THRESHOLD`:下行平均速率达阈值 → 正在流式接收(SSE)。
///   keepalive / HTTP2 PING 是「小且周期」,平均速率低,被阈值过滤掉;SSE 是「持续且高频」,
///   平均速率高,得以识别。
/// - `up_burst >= REQUEST_BURST_MIN`:本周期上行字节达阈值 → 刚发出请求体(突发),
///   用来覆盖「请求已发出、首 token 未到」的空档,兜底启动延迟。
pub fn is_effective_activity(down_rate: f64, up_burst: u64) -> bool {
    down_rate >= DOWN_RATE_ACTIVE_THRESHOLD || up_burst >= REQUEST_BURST_MIN
}

/// 由存活连接数 + 距上次有效活动的时长判三态(DESIGN §14.4 步骤 5)。
///
/// - 无 alive 连接 → `Offline`。
/// - 距上次有效活动 < `ACTIVE_WINDOW` → `Active`。
/// - 否则 → `Idle`。
pub fn classify(alive_conn_count: usize, since_effective: Duration) -> LinkState {
    if alive_conn_count == 0 {
        LinkState::Offline
    } else if since_effective < ACTIVE_WINDOW {
        LinkState::Active
    } else {
        LinkState::Idle
    }
}

/// 机器级整体状态:任一 Active → Active;否则任一 Idle → Idle;否则 Offline。
///
/// 空列表(没有任何 session)归为 Offline(DESIGN §10,沿用 v1)。
pub fn overall(sessions: &[Session]) -> LinkState {
    let mut any_idle = false;
    for s in sessions {
        match s.state {
            LinkState::Active => return LinkState::Active,
            LinkState::Idle => any_idle = true,
            LinkState::Offline => {}
        }
    }
    if any_idle {
        LinkState::Idle
    } else {
        LinkState::Offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— is_effective_activity:阈值过滤(L1)+ 下行速率窗口 / 上行突发(L2)(DESIGN §14.5)——

    /// keepalive:下行平均速率很低(如 20 B/s)、无上行突发 → 非有效活动(false)。
    /// 这正是 v2 要修正 v1 的核心:保活流量不再被误判为 Active。
    #[test]
    fn keepalive_low_rate_not_effective() {
        // 20 B/s 远低于 DOWN_RATE_ACTIVE_THRESHOLD(256.0);上行 0 < REQUEST_BURST_MIN(1024)。
        assert!(!is_effective_activity(20.0, 0));
    }

    /// SSE 流式响应:下行平均速率明显高于阈值 → 有效活动(true)。
    #[test]
    fn sse_high_down_rate_is_effective() {
        // 阈值的 4 倍(平台无关写法,适配 Windows 256 / macOS 2048)→ true,即便上行无突发。
        assert!(is_effective_activity(DOWN_RATE_ACTIVE_THRESHOLD * 4.0, 0));
    }

    /// 上行请求突发:本周期上行字节达阈值(如 4096)→ 有效活动(true)。
    /// 覆盖「请求已发出、首 token 未到」的空档。
    #[test]
    fn up_request_burst_is_effective() {
        // 下行为 0,但上行 4096 ≥ REQUEST_BURST_MIN(1024) → true。
        assert!(is_effective_activity(0.0, 4096));
    }

    /// 下行低速 + 上行不足:两条件都不满足 → 非有效活动(false)。
    #[test]
    fn both_low_not_effective() {
        // 下行 100 B/s < 256.0,上行 512 < 1024 → false。
        assert!(!is_effective_activity(100.0, 512));
    }

    /// 边界:下行速率恰好等于阈值 → 有效(>= 含等号)。
    #[test]
    fn down_rate_at_threshold_is_effective() {
        assert!(is_effective_activity(DOWN_RATE_ACTIVE_THRESHOLD, 0));
    }

    /// 边界:上行突发恰好等于阈值 → 有效(>= 含等号)。
    #[test]
    fn up_burst_at_threshold_is_effective() {
        assert!(is_effective_activity(0.0, REQUEST_BURST_MIN));
    }

    // —— classify:三态边界(DESIGN §14.5)——

    /// 无 alive 连接 → Offline(无论 since_effective 多小)。
    #[test]
    fn no_alive_conn_is_offline() {
        assert_eq!(classify(0, Duration::ZERO), LinkState::Offline);
        // 即便 since 很大也仍是 Offline(连接数优先)。
        assert_eq!(classify(0, Duration::from_secs(60)), LinkState::Offline);
    }

    /// 有连接、距上次有效活动 < ACTIVE_WINDOW → Active。
    #[test]
    fn recent_effective_is_active() {
        let since = ACTIVE_WINDOW / 2;
        assert_eq!(classify(1, since), LinkState::Active);
        // 边界:since = 0 也是 Active。
        assert_eq!(classify(2, Duration::ZERO), LinkState::Active);
    }

    /// 有连接、距上次有效活动 >= ACTIVE_WINDOW → Idle。
    #[test]
    fn stale_effective_is_idle() {
        // 边界:since 恰好等于 ACTIVE_WINDOW → Idle(< 才是 Active)。
        assert_eq!(classify(1, ACTIVE_WINDOW), LinkState::Idle);
        assert_eq!(
            classify(1, ACTIVE_WINDOW + Duration::from_secs(1)),
            LinkState::Idle
        );
    }

    // —— overall:聚合优先级(沿用 v1 用例,DESIGN §14.5)——

    /// 构造一个测试用 Session(只关心 state;其余字段填占位)。
    fn session(pid: u32, state: LinkState) -> Session {
        Session {
            pid,
            cmdline: format!("claude pid={pid}"),
            state,
            conn_count: 0,
            rate_in: 0,
            rate_out: 0,
        }
    }

    /// 任一 Active → 整体 Active(优先级最高)。
    #[test]
    fn overall_any_active_is_active() {
        let sessions = vec![
            session(1, LinkState::Idle),
            session(2, LinkState::Active),
            session(3, LinkState::Offline),
        ];
        assert_eq!(overall(&sessions), LinkState::Active);
    }

    /// 无 Active 但有 Idle → 整体 Idle。
    #[test]
    fn overall_any_idle_is_idle() {
        let sessions = vec![
            session(1, LinkState::Offline),
            session(2, LinkState::Idle),
            session(3, LinkState::Offline),
        ];
        assert_eq!(overall(&sessions), LinkState::Idle);
    }

    /// 全 Offline → 整体 Offline。
    #[test]
    fn overall_all_offline_is_offline() {
        let sessions = vec![
            session(1, LinkState::Offline),
            session(2, LinkState::Offline),
        ];
        assert_eq!(overall(&sessions), LinkState::Offline);
    }

    /// 空 sessions → 整体 Offline(沿用 v1)。
    #[test]
    fn overall_empty_is_offline() {
        assert_eq!(overall(&[]), LinkState::Offline);
    }
}
