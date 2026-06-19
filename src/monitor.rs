//! `Engine`:消费 EngineMsg、维护 conns 表、定时评估并推送托盘更新(DESIGN §9.5)。
//!
//! conns 表只被 Engine 线程独占访问,无需加锁;唯一跨线程共享的是 claude_pids。

use crate::model::*;
use crate::platform::TcpSnapshot;
use crate::state;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Engine 主循环心跳间隔(DESIGN §19.2)。每隔此时长打一条心跳日志,
/// 报告自上次心跳以来处理的 `EngineMsg::Net` 数与当前 conns/procs 规模,
/// 用于诊断「运行一段时间后日志停止」:心跳停 = Engine 线程已死(配合 panic hook 定因);
/// 心跳在但 N=0 = ETW 事件不再到达;心跳在且 N>0 但状态不变 = 三态判定/阈值问题。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// 汇入 Engine 的统一消息(所有事件源的单一入口)。
pub enum EngineMsg {
    Net(NetEvent),
    Procs(Vec<ClaudeProc>),
    // `Quit` 属 DESIGN §9.5 锁定契约。当前 wiring 选择「托盘 Quit → 主线程 ControlFlow::Exit,
    // 其余为 daemon 线程随进程退出」的方案(DESIGN §3 允许),Engine 也在 channel 断开时退出,
    // 故没有显式发送 Quit 的路径;保留该变体以符合契约并便于未来主动收尾,显式允许 dead_code。
    #[allow(dead_code)]
    Quit,
}

/// 每个 Claude 进程的跨评估周期流量状态(Engine 独占,单线程无锁,DESIGN §14.3)。
///
/// v2 用它替换 v1 的 `prev_totals: HashMap<u32,(u64,u64)>`:除了上次累计字节,还维护一个
/// 下行 delta 的滑动窗口(用于算下行平均速率,逼近 SSE 流)与「最后一次有效活动时刻」
/// (驱动 Active 窗口)。
struct PidFlow {
    /// 上次评估时该 pid 全部连接的累计入字节。
    prev_in: u64,
    /// 上次评估时该 pid 全部连接的累计出字节。
    prev_out: u64,
    /// 最近 N=RATE_WINDOW/EVAL_INTERVAL 个评估周期各自的下行 delta(用于算下行平均速率)。
    down_buckets: VecDeque<u64>,
    /// 对称 down_buckets:最近 N 个评估周期各自的上行 delta(DESIGN §21.3)。
    /// 仅用于「展示速率」走 2s 窗口平均(rate_out),不参与 burst 检测(那仍用瞬时 delta_out)。
    up_buckets: VecDeque<u64>,
    /// 最后一次「有效活动」时刻(驱动 Active 窗口;classify 用 now - last_effective 判三态)。
    last_effective: Instant,
}

impl PidFlow {
    /// 新 pid 首次出现时构造:`last_effective` 初始化为 `now - ACTIVE_WINDOW`,
    /// 即默认非 Active,避免新进程一出现就误判 Active(DESIGN §14.3)。
    fn new(now: Instant) -> Self {
        Self {
            prev_in: 0,
            prev_out: 0,
            down_buckets: VecDeque::new(),
            up_buckets: VecDeque::new(),
            // now 早于 ACTIVE_WINDOW 时(测试/启动边界)用 now 兜底,since 即 0 → 会判 Active;
            // 真实运行中 now 远晚于进程启动,checked_sub 必成功,默认非 Active。
            last_effective: now.checked_sub(ACTIVE_WINDOW).unwrap_or(now),
        }
    }
}

/// 引擎:单线程串行消费 EngineMsg,无数据竞争。
pub struct Engine {
    conns: HashMap<ConnKey, ConnState>,
    procs: HashMap<u32, String>,            // 当前存活 Claude 进程
    claude_pids: Arc<RwLock<HashSet<u32>>>, // 与 ETW 共享
    // v2:per-pid 跨周期流量状态,替换 v1 的 prev_totals(DESIGN §14.3)。
    flows: HashMap<u32, PidFlow>,
    last_eval: Instant,
    snapshot: Box<dyn TcpSnapshot>, // 给新 pid 补连接
}

impl Engine {
    /// 构造引擎。
    pub fn new(claude_pids: Arc<RwLock<HashSet<u32>>>, snapshot: Box<dyn TcpSnapshot>) -> Self {
        Self {
            conns: HashMap::new(),
            procs: HashMap::new(),
            claude_pids,
            flows: HashMap::new(),
            last_eval: Instant::now(),
            snapshot,
        }
    }

    /// 更新 conns:Connect 插入 / Data 累加并刷新 last_activity / Disconnect 置 alive=false。
    ///
    /// 所有事件都顺带刷新 `last_seen`(用于 GC)。`Instant` 不进 channel,统一在此用
    /// `Instant::now()` 标记(DESIGN §11.7),避免跨线程时钟语义混乱。
    fn apply_net(&mut self, ev: NetEvent) {
        let now = Instant::now();
        match ev {
            // 新连接:不存在则插入;已存在(快照补发与 ETW 可能重复)则复用旧累计,
            // 仅刷新 alive/last_seen,避免把已有字节清零。
            NetEvent::Connect { pid, key } => {
                self.conns
                    .entry(key)
                    .and_modify(|c| {
                        c.pid = pid;
                        c.alive = true;
                        c.last_seen = now;
                    })
                    .or_insert_with(|| ConnState {
                        pid,
                        key,
                        bytes_in: 0,
                        bytes_out: 0,
                        last_activity: now,
                        alive: true,
                        last_seen: now,
                    });
            }
            // 数据收发:累加字节,刷新 last_activity(决定 Active)与 last_seen。
            // 若该连接尚未通过 Connect 入表(ETW 可能先收到 Send/Recv),则按需补建,
            // 保证「能判断三态」这一硬目标(DESIGN §9.3 末注)。
            NetEvent::Data {
                pid,
                key,
                inbound,
                outbound,
            } => {
                let c = self.conns.entry(key).or_insert_with(|| ConnState {
                    pid,
                    key,
                    bytes_in: 0,
                    bytes_out: 0,
                    last_activity: now,
                    alive: true,
                    last_seen: now,
                });
                c.pid = pid;
                c.bytes_in = c.bytes_in.saturating_add(inbound);
                c.bytes_out = c.bytes_out.saturating_add(outbound);
                c.last_activity = now;
                c.last_seen = now;
                c.alive = true;
            }
            // 断开:置 alive=false,保留累计字节等待 GC(超 CONN_GC_TTL 再清)。
            NetEvent::Disconnect { pid, key } => {
                if let Some(c) = self.conns.get_mut(&key) {
                    c.pid = pid;
                    c.alive = false;
                    c.last_seen = now;
                }
            }
        }
    }

    /// 更新 procs + 写回 claude_pids;对新增 pid 调 snapshot 补 Connect。
    ///
    /// 进程发现是一次「全量快照」:`list` 即当前所有存活的 Claude 进程,故直接重建
    /// `procs` 与 `claude_pids`。新增 pid(本轮有、上轮无)需用 TcpSnapshot 补已存在的
    /// ESTABLISHED 连接,这样不依赖 ETW 也能从 Offline 进到 Idle(降级两态模式)。
    fn refresh_procs(&mut self, list: Vec<ClaudeProc>) {
        // 新的存活进程表。
        let mut new_procs: HashMap<u32, String> = HashMap::with_capacity(list.len());
        // 本轮新增(上一轮不存在)的 pid,稍后补连接。
        let mut added: Vec<u32> = Vec::new();
        for p in list {
            if !self.procs.contains_key(&p.pid) {
                added.push(p.pid);
            }
            new_procs.insert(p.pid, p.cmdline);
        }
        self.procs = new_procs;

        // 写回共享过滤集合(供 ETW 回调过滤热路径)。一次性重建,持锁时间最短。
        {
            let mut pids = match self.claude_pids.write() {
                Ok(g) => g,
                // 锁中毒(某线程 panic)极少见;退而清空再填,避免整体卡死。
                Err(poisoned) => poisoned.into_inner(),
            };
            pids.clear();
            pids.extend(self.procs.keys().copied());
        }

        // 对新增 pid 做一次 TCP 快照,把已存在的连接补成 Connect 入表。
        if !added.is_empty() {
            let added_set: HashSet<u32> = added.into_iter().collect();
            for (pid, key) in self.snapshot.snapshot(&added_set) {
                self.apply_net(NetEvent::Connect { pid, key });
            }
        }
    }

    /// 清理 dead 进程的连接、超 CONN_GC_TTL 的 !alive 连接。
    ///
    /// 两条清理规则:
    /// 1. 连接所属 pid 已不在 `procs`(进程退出)→ 立即删除该连接。
    /// 2. 连接 `!alive`(已收到 Disconnect)且距 `last_seen` 超过 `CONN_GC_TTL` → 删除。
    ///
    /// `alive` 但进程仍在的连接一律保留(即使长时间静默,也只是 Idle,不清)。
    fn gc(&mut self, now: Instant) {
        let procs = &self.procs;
        self.conns.retain(|_key, c| {
            // 进程已消失:无论 alive 与否,连接都无意义,清理。
            if !procs.contains_key(&c.pid) {
                return false;
            }
            // 已断开且超过 TTL:清理;否则保留。
            if !c.alive && now.saturating_duration_since(c.last_seen) >= CONN_GC_TTL {
                return false;
            }
            true
        });
    }

    /// 评估:对每个存活 pid 跑 v2 判定算法(DESIGN §14.4),返回 (sessions, 整体状态)。
    ///
    /// v1 的 `state::evaluate` 大函数因需跨周期状态(`PidFlow`)而由本循环取代;判定的纯部分
    /// (`is_effective_activity` / `classify` / `overall`)仍调 `state` 模块。
    ///
    /// 逐 pid 算法(§14.4):
    /// 1. 聚合该 pid 全部连接 → (alive_conn_count, total_in, total_out)。
    /// 2. delta = total - prev;刷新 prev。
    /// 3. push delta_in 进 down_buckets(维持 N 个桶);down_rate = sum / RATE_WINDOW。
    /// 4. effective = is_effective_activity(down_rate, delta_out);若 true 刷新 last_effective。
    /// 5. state = classify(alive_conn_count, now - last_effective)。
    /// 6. 展示速率 rate_in/out = 2s 窗口平均(sum(buckets)/RATE_WINDOW,DESIGN §21.3)。
    pub fn evaluate(&mut self, now: Instant) -> (Vec<Session>, LinkState) {
        // 下行速率窗口桶数 N = RATE_WINDOW / EVAL_INTERVAL(至少 1 桶,防呆)。
        let bucket_cap = (RATE_WINDOW.as_secs_f64() / EVAL_INTERVAL.as_secs_f64()).round() as usize;
        let bucket_cap = bucket_cap.max(1);
        let rate_window_secs = RATE_WINDOW.as_secs_f64();

        // 步骤 1:逐 pid 聚合全部连接(含已断开的,累计字节是历史量,用于算 delta/速率)。
        // 同 v1:为每个存活进程都建一个聚合条目,即便它当前没有任何连接,也产出 Offline Session。
        struct Agg {
            alive_conn_count: usize,
            total_in: u64,
            total_out: u64,
        }
        let mut agg: HashMap<u32, Agg> = HashMap::with_capacity(self.procs.len());
        for &pid in self.procs.keys() {
            agg.entry(pid).or_insert(Agg {
                alive_conn_count: 0,
                total_in: 0,
                total_out: 0,
            });
        }
        for conn in self.conns.values() {
            // 只对当前存活的进程产出 Session;非存活进程的连接由 gc 处理,这里忽略。
            let Some(entry) = agg.get_mut(&conn.pid) else {
                continue;
            };
            entry.total_in = entry.total_in.saturating_add(conn.bytes_in);
            entry.total_out = entry.total_out.saturating_add(conn.bytes_out);
            if conn.alive {
                entry.alive_conn_count += 1;
            }
        }

        let mut sessions: Vec<Session> = Vec::with_capacity(agg.len());

        for (&pid, a) in &agg {
            // 取/建该 pid 的跨周期流量状态。新 pid 默认非 Active(last_effective = now - ACTIVE_WINDOW)。
            let flow = self.flows.entry(pid).or_insert_with(|| PidFlow::new(now));

            // 步骤 2:delta = 本次累计 - 上次累计(饱和减防回绕:进程重启/连接 GC 后累计可能变小)。
            let delta_in = a.total_in.saturating_sub(flow.prev_in);
            let delta_out = a.total_out.saturating_sub(flow.prev_out);
            flow.prev_in = a.total_in;
            flow.prev_out = a.total_out;

            // 步骤 3:把本周期下行/上行 delta 推入各自滑动窗口,维持 N 个桶;
            // 算下行平均速率(B/s)。上行窗口(up_buckets)对称维护,仅供展示速率(DESIGN §21.3)。
            flow.down_buckets.push_back(delta_in);
            while flow.down_buckets.len() > bucket_cap {
                flow.down_buckets.pop_front();
            }
            flow.up_buckets.push_back(delta_out);
            while flow.up_buckets.len() > bucket_cap {
                flow.up_buckets.pop_front();
            }
            let down_sum: u64 = flow.down_buckets.iter().copied().sum();
            let down_rate = down_sum as f64 / rate_window_secs;
            let up_sum: u64 = flow.up_buckets.iter().copied().sum();

            // 步骤 4:有效活动判定(下行流 或 上行突发);命中则刷新 last_effective。
            let effective = state::is_effective_activity(down_rate, delta_out);
            if effective {
                flow.last_effective = now;
            }

            // 步骤 5:三态判定。无 alive 连接 → Offline;距上次有效活动 < ACTIVE_WINDOW → Active;否则 Idle。
            let since_effective = now.saturating_duration_since(flow.last_effective);
            let link_state = state::classify(a.alive_conn_count, since_effective);

            // 步骤 6:展示用速率改为 2s 窗口平均(与 active 判定同口径,DESIGN §21.3)。
            // SSE 阵发下许多 500ms 拍为 0,瞬时 delta/dt 会把 Active 抖成 ↓0↑0;
            // 用窗口平均抹平。rate_in 即上面算好的 down_rate;rate_out = sum(up_buckets)/RATE_WINDOW。
            // burst 检测仍用瞬时 delta_out(步骤 4 不变),这里只动展示口径。
            let rate_in = down_rate as u64;
            let rate_out = (up_sum as f64 / rate_window_secs) as u64;

            // 每 pid debug 日志:供 e2e 校准阈值(DESIGN §14.7)。
            // 打印 pid、下行平均速率、上行 delta、是否有效、最终三态。
            tracing::debug!(
                pid,
                down_rate,
                delta_out,
                effective,
                state = ?link_state,
                "pid 评估"
            );

            let cmdline = self.procs.get(&pid).cloned().unwrap_or_default();
            sessions.push(Session {
                pid,
                cmdline,
                state: link_state,
                conn_count: a.alive_conn_count,
                rate_in,
                rate_out,
            });
        }

        // 按 pid 排序,稳定输出(沿用 v1)。
        sessions.sort_by_key(|s| s.pid);

        // 清理已不在 procs 的 pid 的 PidFlow,避免泄漏(DESIGN §14.3:pid 不在 procs 时随 gc 移除)。
        self.flows.retain(|pid, _| self.procs.contains_key(pid));

        self.last_eval = now;
        let overall = state::overall(&sessions);
        (sessions, overall)
    }

    /// 主循环:在独立线程调用。rx 收 EngineMsg,`recv_timeout(EVAL_INTERVAL)` 兼当 tick。
    ///
    /// - 收到 `Net`/`Procs` → 调对应 apply,**不立即评估**(避免每个事件都全量算一遍);
    ///   只在到达评估节拍时统一 gc + evaluate。
    /// - `recv_timeout` 超时(无消息) → 到节拍,评估并推托盘。
    /// - 即便消息持续不断没触发超时,也用 `last_eval` 判断是否已过 `EVAL_INTERVAL`,
    ///   到点就评估一次,保证托盘按节拍刷新。
    /// - 收到 `Quit` → break,函数返回(线程随之结束)。
    pub fn run(
        mut self,
        rx: Receiver<EngineMsg>,
        mut on_update: impl FnMut(Vec<Session>, LinkState),
    ) {
        // 让第一帧尽快出来:把 last_eval 拨到一个 EVAL_INTERVAL 之前,
        // 这样首次收到任意消息或首次超时即评估。
        self.last_eval = Instant::now()
            .checked_sub(EVAL_INTERVAL)
            .unwrap_or_else(Instant::now);

        // 心跳局部状态(DESIGN §19.2):net_count 累计自上次心跳以来处理的 Net 事件数,
        // last_hb 记上次心跳时刻;每过 HEARTBEAT_INTERVAL 打一条并清零。
        let mut net_count: u64 = 0;
        let mut last_hb = Instant::now();

        loop {
            match rx.recv_timeout(EVAL_INTERVAL) {
                Ok(EngineMsg::Net(ev)) => {
                    net_count += 1;
                    self.apply_net(ev);
                }
                Ok(EngineMsg::Procs(list)) => self.refresh_procs(list),
                Ok(EngineMsg::Quit) => break,
                // 通道关闭(所有发送端 drop):等同退出,清理收尾。
                Err(RecvTimeoutError::Disconnected) => break,
                // 超时:正常 tick,落到下面统一评估。
                Err(RecvTimeoutError::Timeout) => {}
            }

            // 到达评估节拍才评估(超时必到点;收到消息时按 last_eval 判断,
            // 避免高频事件下每条都全量评估)。
            let now = Instant::now();
            if now.saturating_duration_since(self.last_eval) >= EVAL_INTERVAL {
                self.gc(now);
                let (sessions, overall) = self.evaluate(now);
                on_update(sessions, overall);
            }

            // 心跳:在评估节拍块之后判断,到点就打一条并重置计数与时间戳(DESIGN §19.2)。
            if now.saturating_duration_since(last_hb) >= HEARTBEAT_INTERVAL {
                tracing::info!(
                    net_events = net_count,
                    conns = self.conns.len(),
                    procs = self.procs.len(),
                    "engine 心跳"
                );
                net_count = 0;
                last_hb = now;
            }
        }
    }
}
