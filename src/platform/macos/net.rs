//! `MacNetMonitor`:轮询 `nettop` 单次快照取 per-pid 实时字节(对应 windows/etw.rs)。
//!
//! **为什么不直连 ntstat**:per-pid 字节的内核源是私有 `com.apple.network.statistics`,
//! 直连订阅需 Apple 私有 entitlement(未签名二进制实测 `ENOENT`);`nettop` 自带该 entitlement。
//!
//! **为什么轮询而非持续流**:`nettop -x -l 0`(持续 logging)在两次采样的间隔里 **busy-spin**
//! ——实测无论 `-s 1` 还是 `-s 5` 都烧满 ~140% CPU(它压根没 sleep)。改用每 `POLL_INTERVAL`
//! fork 一次 `nettop -l 1`(单次快照,实测 ~40ms 就退出、不空转),自己维护上次累计算 delta,
//! 再由我们 `thread::sleep` 真正休眠,占空比 ~4%。pid 过滤靠 `-p <claude pids>`,每轮用最新
//! 集合,天然支持进程动态增减。
//!
//! 数据映射(与 etw.rs 同语义):连接行四元组 → ConnKey;累计差 → Data;首见 → Connect;
//! 轮次间消失 → Disconnect。

use crate::model::{ConnKey, NetEvent};
use crate::platform::NetMonitor;
use crossbeam_channel::Sender;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// 轮询采样间隔。每轮只 fork 一次 `nettop -l 1`(~40ms),其余时间真 sleep。
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// nettop 命令失败时的退避(防持续出错时 busy 重试)。
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// macOS 实时网络监控器(基于 nettop 单次快照轮询)。
pub struct MacNetMonitor {}

impl MacNetMonitor {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for MacNetMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl NetMonitor for MacNetMonitor {
    fn start(
        &mut self,
        claude_pids: Arc<RwLock<HashSet<u32>>>,
        tx: Sender<NetEvent>,
    ) -> anyhow::Result<()> {
        // 同步试一次(空 pid 也能跑):失败即返回 Err,让上层降级两态(如 PATH 无 nettop)。
        nettop_snapshot(&[])
            .map_err(|e| anyhow::anyhow!("nettop 不可用(PATH 里有 nettop?): {e}"))?;

        // 轮询线程随进程存活(daemon)。无 -l 0 持续流,故无 busy-spin、也无子进程长驻孤儿。
        std::thread::Builder::new()
            .name("seecn-nettop".into())
            .spawn(move || poll_loop(claude_pids, tx))
            .map_err(|e| anyhow::anyhow!("无法创建 nettop 轮询线程: {e}"))?;

        Ok(())
    }
}

/// 跑一次 `nettop -n -x -l 1 -p <pids>`,阻塞到命令结束(~40ms),返回 stdout 文本。
fn nettop_snapshot(pids: &[u32]) -> std::io::Result<String> {
    let mut cmd = Command::new("nettop");
    cmd.args(["-n", "-x", "-l", "1"]);
    for &pid in pids {
        cmd.arg("-p").arg(pid.to_string());
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "nettop 退出码 {:?}",
            out.status.code()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 轮询循环:每 `POLL_INTERVAL` 拍一次快照、算 delta、推 NetEvent,然后真 sleep。
fn poll_loop(claude_pids: Arc<RwLock<HashSet<u32>>>, tx: Sender<NetEvent>) {
    // (pid, key) → 上次累计 (in, out),用于算 delta。
    let mut prev: HashMap<(u32, ConnKey), (u64, u64)> = HashMap::new();
    // 当前活跃连接集(已发 Connect、未发 Disconnect)。
    let mut known: HashSet<(u32, ConnKey)> = HashSet::new();

    loop {
        let pids: Vec<u32> = match claude_pids.read() {
            Ok(g) => g.iter().copied().collect(),
            Err(_) => Vec::new(), // 锁中毒:本轮当作无 pid,保守跳过
        };

        // 无 Claude 进程:把存量连接全 Disconnect、清状态,sleep 后下轮再看。
        if pids.is_empty() {
            for e in known.drain() {
                let _ = tx.send(NetEvent::Disconnect { pid: e.0, key: e.1 });
            }
            prev.clear();
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        match nettop_snapshot(&pids) {
            Ok(text) => {
                let seen = process_snapshot(&text, &mut prev, &mut known, &tx);
                // known 中本轮未见的连接 → 已断开。
                let gone: Vec<(u32, ConnKey)> = known.difference(&seen).copied().collect();
                for e in gone {
                    known.remove(&e);
                    prev.remove(&e);
                    let _ = tx.send(NetEvent::Disconnect { pid: e.0, key: e.1 });
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                tracing::warn!("nettop 快照失败,退避后重试: {e}");
                std::thread::sleep(ERROR_BACKOFF);
            }
        }
    }
}

/// 解析单次快照:进程行更新归属 pid,连接行用累计差产出 Connect/Data。返回本轮见到的连接集。
///
/// 因 `-p` 已把输出锁定在 Claude 进程,无需再按 pid 过滤;仍解析进程行以归属连接。
fn process_snapshot(
    text: &str,
    prev: &mut HashMap<(u32, ConnKey), (u64, u64)>,
    known: &mut HashSet<(u32, ConnKey)>,
    tx: &Sender<NetEvent>,
) -> HashSet<(u32, ConnKey)> {
    let mut seen: HashSet<(u32, ConnKey)> = HashSet::new();
    let mut current_pid: Option<u32> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let first = match fields.next() {
            Some(f) => f,
            None => continue,
        };
        if first == "time" {
            continue; // 表头行
        }
        // 数据行:first 是时间戳;下一字段区分进程行 / 连接行。
        let f2 = match fields.next() {
            Some(f) => f,
            None => continue,
        };

        if f2.starts_with("tcp") {
            // 连接行:tcp4/tcp6 LOCAL<->REMOTE iface state cum_in cum_out ...
            let pid = match current_pid {
                Some(p) => p,
                None => continue,
            };
            let addrs = match fields.next() {
                Some(a) => a,
                None => continue,
            };
            let key = match parse_conn_key(addrs) {
                Some(k) => k,
                None => continue, // 尽力而为:解析失败跳过该行
            };
            let _iface = fields.next();
            let _state = fields.next();
            let cum_in = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let cum_out = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let entry = (pid, key);
            seen.insert(entry);
            if known.insert(entry) {
                // 首见:发 Connect,记累计基线;本轮不发 Data(delta 未知)。
                let _ = tx.send(NetEvent::Connect { pid, key });
                prev.insert(entry, (cum_in, cum_out));
            } else {
                // 已知:算 delta(饱和减防连接重建后累计回绕),更新基线;>0 才发 Data。
                let (p_in, p_out) = prev.get(&entry).copied().unwrap_or((cum_in, cum_out));
                let d_in = cum_in.saturating_sub(p_in);
                let d_out = cum_out.saturating_sub(p_out);
                prev.insert(entry, (cum_in, cum_out));
                if d_in > 0 || d_out > 0 {
                    let _ = tx.send(NetEvent::Data {
                        pid,
                        key,
                        inbound: d_in,
                        outbound: d_out,
                    });
                }
            }
        } else if let Some(pid) = parse_proc_pid(f2) {
            current_pid = Some(pid);
        }
    }

    seen
}

/// 解析进程行第二字段 "a.b.c.<pid>" 末段为 pid。
fn parse_proc_pid(f: &str) -> Option<u32> {
    f.rsplit('.').next()?.parse::<u32>().ok()
}

/// 解析 "LOCAL<->REMOTE" 为 ConnKey(`ip:port`)。
fn parse_conn_key(s: &str) -> Option<ConnKey> {
    let (l, r) = s.split_once("<->")?;
    Some(ConnKey {
        local: l.parse::<SocketAddr>().ok()?,
        remote: r.parse::<SocketAddr>().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_pid_from_nettop_field() {
        assert_eq!(parse_proc_pid("2.1.185.86519"), Some(86519));
        assert_eq!(parse_proc_pid("claude"), None);
    }

    #[test]
    fn conn_key_ipv4() {
        let k = parse_conn_key("172.19.0.1:56444<->160.79.104.10:443").unwrap();
        assert_eq!(k.local, "172.19.0.1:56444".parse::<SocketAddr>().unwrap());
        assert_eq!(k.remote, "160.79.104.10:443".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn conn_key_garbage_is_none() {
        assert!(parse_conn_key("not-an-addr").is_none());
        assert!(parse_conn_key("1.2.3.4:5<->garbage").is_none());
    }
}
