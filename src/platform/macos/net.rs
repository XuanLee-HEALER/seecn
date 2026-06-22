//! `MacNetMonitor`:借 `nettop` 常驻子进程取 per-pid 实时字节(对应 windows/etw.rs)。
//!
//! 为什么不直连 ntstat:macOS 的 per-pid 字节源是私有 `com.apple.network.statistics`
//! (ntstat)kernel control,直连订阅需 Apple 私有 entitlement
//! `com.apple.private.network.statistics`。本机实测:未签名二进制对 `ADD_ALL_SRCS` 一律
//! `ENOENT`;`nettop` 自带该 entitlement(codesign 实证)。故借 nettop:fork 一个常驻
//! `nettop -d`(delta 模式)进程,逐行解析 stdout → 组 NetEvent。等价于 ETW 回调推送:
//! nettop 内部即订阅 ntstat 的推送封装,delta 由它算好。
//!
//! 数据映射(与 etw.rs 同语义):连接行四元组 → ConnKey;本周期 delta 字节 → Data;
//! 连接首见 → Connect;周期间消失 → Disconnect。pid 过滤走共享 `claude_pids`(同 ETW 模型),
//! 因此不加 `-p`(全局监控 + 解析时按 pid 过滤),天然适应 Claude 进程集合的动态增减。

use crate::model::{ConnKey, NetEvent};
use crate::platform::NetMonitor;
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// nettop 采样间隔(秒)。1s 足够支撑三态(ACTIVE_WINDOW=1500ms);窗口平均会抹平阵发。
const NETTOP_INTERVAL_SECS: &str = "1";

/// nettop 崩溃后的重启退避:初始 1s,指数翻倍到 30s 上限(防持续秒崩 busy-loop)。
const NETTOP_RESTART_MIN: Duration = Duration::from_secs(1);
const NETTOP_RESTART_MAX: Duration = Duration::from_secs(30);
/// 上一条 nettop 存活 ≥ 此时长才崩 → 视为偶发,重置退避快速重起。
const NETTOP_STABLE: Duration = Duration::from_secs(30);

/// 一个运行中的 nettop 子进程 + 它的 stdout。
struct NettopProc {
    child: Child,
    stdout: ChildStdout,
}

/// 启动一个常驻 nettop delta 流子进程。
/// `-n` 数字地址 / `-x` 纯文本(可管道解析)/ `-d` 增量 / `-s` 间隔 / `-l 0` 无限。
fn spawn_nettop() -> std::io::Result<NettopProc> {
    let mut child = Command::new("nettop")
        .args(["-n", "-x", "-d", "-s", NETTOP_INTERVAL_SECS, "-l", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("nettop stdout 不可用"))?;
    Ok(NettopProc { child, stdout })
}

/// macOS 实时网络监控器(基于 nettop 常驻流)。
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
        // 同步起第一个 nettop:失败即返回 Err,让上层据此降级两态(如 PATH 无 nettop)。
        // 不加 -p:全局监控,解析时按 claude_pids 过滤(贴 ETW 模型,支持 pid 集合动态增减)。
        let initial = spawn_nettop()
            .map_err(|e| anyhow::anyhow!("启动 nettop 失败(PATH 里有 nettop?): {e}"))?;

        // 监督线程:解析当前 nettop 直到它退出,然后退避重启(崩溃自救)。线程随进程存活(daemon)。
        // 孤儿无需主动清理:主进程一死,nettop 的 stdout 读端关闭,它下个采样周期写 stdout 即吃
        // SIGPIPE 自杀(实测 SIGKILL 主进程后 nettop ~1s 内自动消失)。
        std::thread::Builder::new()
            .name("seecn-nettop".into())
            .spawn(move || supervise(initial, claude_pids, tx))
            .map_err(|e| anyhow::anyhow!("无法创建 nettop 监督线程: {e}"))?;

        Ok(())
    }
}

/// 监督循环:解析当前 nettop 直到流断(进程退出/崩溃),kill+wait 回收,退避后重启。
///
/// nettop 崩溃**不致命**:本循环自愈重启;重启期间 Engine 靠存量连接 + GC 维持,
/// proc + snapshot 仍给 Offline/Idle。退避初始 1s 指数到 30s 上限;若上条 nettop 已稳定
/// 存活(≥STABLE)再崩,视为偶发、重置退避快速重起。
fn supervise(initial: NettopProc, claude_pids: Arc<RwLock<HashSet<u32>>>, tx: Sender<NetEvent>) {
    let mut current = initial;
    let mut backoff = NETTOP_RESTART_MIN;

    loop {
        let started = Instant::now();
        // 阻塞解析,直到 nettop 退出(stdout EOF)。
        run_parse_loop(current.stdout, &claude_pids, &tx);
        let _ = current.child.kill();
        let _ = current.child.wait();

        // 稳定运行后才崩 → 偶发,重置退避;短命连崩 → 保持递增退避避免 busy-loop。
        if started.elapsed() >= NETTOP_STABLE {
            backoff = NETTOP_RESTART_MIN;
        }
        tracing::warn!(backoff_secs = backoff.as_secs(), "nettop 退出,退避后重启");

        // 重启:成功回到解析;失败继续退避重试(daemon 一直尝试)。
        loop {
            std::thread::sleep(backoff);
            backoff = backoff.saturating_mul(2).min(NETTOP_RESTART_MAX);
            match spawn_nettop() {
                Ok(next) => {
                    current = next;
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        backoff_secs = backoff.as_secs(),
                        "nettop 重启失败,退避重试: {e}"
                    );
                }
            }
        }
    }
}

/// 解析 nettop `-x -d` 的流。状态机维护「当前进程行归属的 pid」与「跨周期已知连接集」,
/// 据此产出 Connect(首见)/ Data(有 delta)/ Disconnect(周期间消失)。
fn run_parse_loop(
    stdout: ChildStdout,
    claude_pids: &Arc<RwLock<HashSet<u32>>>,
    tx: &Sender<NetEvent>,
) {
    let reader = BufReader::new(stdout);

    let mut current_pid: Option<u32> = None;
    let mut current_is_claude = false;
    // 当前活跃连接集(已发过 Connect、未发 Disconnect)。
    let mut known: HashSet<(u32, ConnKey)> = HashSet::new();
    // 本采样周期见到的连接集(与 known diff 出已消失的 → Disconnect)。
    let mut seen: HashSet<(u32, ConnKey)> = HashSet::new();
    let mut in_cycle = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // 流读出错 = nettop 退出
        };
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }

        let mut fields = trimmed.split_whitespace();
        let first = match fields.next() {
            Some(f) => f,
            None => continue,
        };

        // 表头行(每周期一行,首字段字面 "time"):周期边界。
        if first == "time" {
            if in_cycle {
                // 上周期结束:known 中本周期未见的连接 → 已断开。
                let gone: Vec<(u32, ConnKey)> = known
                    .iter()
                    .filter(|e| !seen.contains(e))
                    .copied()
                    .collect();
                for e in gone {
                    known.remove(&e);
                    let _ = tx.send(NetEvent::Disconnect { pid: e.0, key: e.1 });
                }
            }
            seen.clear();
            current_pid = None;
            current_is_claude = false;
            in_cycle = true;
            continue;
        }

        // 数据行:first 是时间戳(已消费);下一字段区分进程行 / 连接行。
        let f2 = match fields.next() {
            Some(f) => f,
            None => continue,
        };

        if f2.starts_with("tcp") {
            // 连接行:tcp4/tcp6 LOCAL<->REMOTE iface state bytes_in bytes_out ...
            if !current_is_claude {
                continue;
            }
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
                None => continue, // 尽力而为:解析失败跳过该行,不 panic
            };
            let _iface = fields.next();
            let _state = fields.next();
            let bin = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let bout = fields
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let entry = (pid, key);
            seen.insert(entry);
            // 首见 → Connect(建立连接存在性,支撑 Idle)。
            if known.insert(entry) {
                let _ = tx.send(NetEvent::Connect { pid, key });
            }
            // 有 delta → Data(支撑 Active)。0/0 不发:连接存在性已由 known 维持,
            // Engine 对 alive 连接即使静默也不 GC,无需 0 字节事件保活(见 monitor.rs gc)。
            if bin > 0 || bout > 0 {
                let _ = tx.send(NetEvent::Data {
                    pid,
                    key,
                    inbound: bin,
                    outbound: bout,
                });
            }
        } else if let Some(pid) = parse_proc_pid(f2) {
            // 进程行:f2 形如 "2.1.185.<pid>",末段为 pid。更新归属 + 是否 Claude。
            current_pid = Some(pid);
            current_is_claude = claude_pids
                .read()
                .map(|g| g.contains(&pid))
                .unwrap_or(false);
        }
    }
}

/// 解析进程行第二字段 "a.b.c.<pid>" 末段为 pid。
/// (连接行已在调用点用 "tcp" 前缀分流,不会进到这里。)
fn parse_proc_pid(f: &str) -> Option<u32> {
    f.rsplit('.').next()?.parse::<u32>().ok()
}

/// 解析 "LOCAL<->REMOTE" 为 ConnKey。LOCAL/REMOTE 为 `ip:port`(IPv4)或 `[ip]:port`(IPv6)。
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
