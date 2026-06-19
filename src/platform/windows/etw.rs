//! `WinNetMonitor`:基于 ferrisetw 的 ETW KernelNetwork 实时监听(DESIGN §9.3,核心)。
//!
//! 监听 `Microsoft-Windows-Kernel-Network`(GUID 7DD42A49-5329-4832-8DFD-43D979153A88),
//! 需管理员权限。回调里先用共享的 `claude_pids` 过滤,只把 Claude 进程的事件
//! 组成 `NetEvent` 投递进 channel,据此让 Engine 判断三态。
//!
//! 设计取舍(DESIGN §9.3 末注):**「能判断三态」是硬目标,连接键(ConnKey)的精确度是次要目标**。
//! 因此地址/端口解析全部走「尽力而为 + 失败兜底为 0」,任何解析失败都不丢弃事件、更不 panic。

use crate::model::{ConnKey, NetEvent};
use crate::platform::NetMonitor;
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, RwLock};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{TraceTrait, UserTrace};
use ferrisetw::EventRecord;

// —— ETW session 残留清理(DESIGN §15)用到的 Win32 原生 API/类型/常量 ——
// 全部来自 windows-sys 0.61 的 Win32_System_Diagnostics_Etw / Win32_Foundation feature。
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND};
use windows_sys::Win32::System::Diagnostics::Etw::{
    ControlTraceW, CONTROLTRACE_HANDLE, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_PROPERTIES,
    WNODE_FLAG_TRACED_GUID,
};

/// Microsoft-Windows-Kernel-Network provider GUID。
pub const KERNEL_NETWORK_PROVIDER_GUID: &str = "7DD42A49-5329-4832-8DFD-43D979153A88";

/// trace session 名称。
const TRACE_NAME: &str = "seecn-net";

/// 订阅的 keyword:只要 IPv4 + IPv6 流量。值经 `logman query providers
/// Microsoft-Windows-Kernel-Network` 核对(IPV4=0x10 / IPV6=0x20),收窄掉用不到的
/// Analytic 通道(0x8000_0000_0000_0000)以降低事件量。
/// 说明:TCP/UDP 不按 keyword 区分,UDP 事件按 event_id 在回调里廉价丢弃;
/// keyword=0 的事件 ETW 总会投递,故收窄后也不会漏掉我们要的 TCP 事件。
const KERNEL_NET_KEYWORDS: u64 = 0x10 | 0x20;

// —— TcpIp 经典 manifest 事件 ID(IPv4 / IPv6 双份),见 DESIGN §9.3 表 ——
// Send:数据发出(outbound)
const EID_SEND_V4: u16 = 10;
const EID_SEND_V6: u16 = 26;
// Recv:数据收到(inbound)
const EID_RECV_V4: u16 = 11;
const EID_RECV_V6: u16 = 27;
// Connect:连接建立
const EID_CONNECT_V4: u16 = 12;
const EID_CONNECT_V6: u16 = 28;
// Disconnect:连接断开
const EID_DISCONNECT_V4: u16 = 13;
const EID_DISCONNECT_V6: u16 = 29;
// Retransmit:重传,并入 Data(outbound),用于刷新活跃度
const EID_RETRANSMIT_V4: u16 = 14;
const EID_RETRANSMIT_V6: u16 = 30;

/// Windows 实时网络监控器。需要管理员权限(ETW Kernel-Network provider)。
pub struct WinNetMonitor {}

impl WinNetMonitor {
    /// 构造监控器。
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for WinNetMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// 预留给 LoggerName / LogFileName 的 wide 缓冲字节数(各一段)。
/// TRACE_NAME 很短,1024 字节(512 个 u16)绰绰有余;固定大小简化布局计算(DESIGN §15.2)。
const NAME_BUF_BYTES: usize = 1024;

/// 启动自愈:停掉任何已存在的同名 ETW realtime session(DESIGN §15.2,修复 B)。
///
/// ETW realtime trace 是系统级持久对象,不随创建进程消亡;进程被强杀 / 不跑 Drop 时
/// 会残留,导致下次同名 `StartTraceW` 报 `ERROR_ALREADY_EXISTS`。这里在 `start()` 最开头
/// 无条件先停一次,做到「先清后建」的 self-heal。
///
/// best-effort 语义:`ERROR_SUCCESS`(已停)与 `ERROR_WMI_INSTANCE_NOT_FOUND`(本就不存在)
/// 都算正常;其它错误仅 `tracing::debug!` 记录。**绝不 panic、绝不阻断后续 start**。
fn stop_stale_trace(name: &str) {
    // ControlTraceW(STOP) 需要一块连续缓冲:
    //   [EVENT_TRACE_PROPERTIES][LoggerName wide buf][LogFileName wide buf]
    // 用一个 #[repr(C)] 结构体表达这块布局,保证三段内存连续且对齐正确。
    #[repr(C)]
    struct TracePropsBuf {
        props: EVENT_TRACE_PROPERTIES,
        logger_name: [u8; NAME_BUF_BYTES],
        log_file_name: [u8; NAME_BUF_BYTES],
    }

    // 整块清零初始化(EVENT_TRACE_PROPERTIES 全字段 zeroed 是合法初值)。
    // SAFETY: TracePropsBuf 仅由 POD 字段(整型、固定数组、本身可 zeroed 的 EVENT_TRACE_PROPERTIES)
    // 组成,全 0 是其合法表示。
    let mut buf: TracePropsBuf = unsafe { core::mem::zeroed() };

    let total_size = core::mem::size_of::<TracePropsBuf>();
    let logger_name_offset = core::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let log_file_name_offset = logger_name_offset + NAME_BUF_BYTES;

    // 按 §15.2 初始化必填字段:
    // - Wnode.BufferSize = 整块字节数(含两段名字缓冲);
    // - Wnode.Flags = WNODE_FLAG_TRACED_GUID(标识这是一个 trace 属性块);
    // - LoggerNameOffset / LogFileNameOffset 指向各自缓冲的起始偏移。
    buf.props.Wnode.BufferSize = total_size as u32;
    buf.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    buf.props.LoggerNameOffset = logger_name_offset as u32;
    buf.props.LogFileNameOffset = log_file_name_offset as u32;

    // session 名转成以 NUL 结尾的 wide 串,作为 InstanceName 传入(按名停止)。
    let name_wide: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();

    // SAFETY: ControlTraceW 的契约:
    //  - tracehandle 传 0 值句柄表示「按 InstanceName 定位 session」;
    //  - instancename 指向上面构造的、以 NUL 结尾、在本调用期间存活的 wide 串;
    //  - properties 指向已正确初始化(BufferSize/Flags/两个 Offset)的连续缓冲,
    //    其大小 >= Wnode.BufferSize,可被内核写回;
    //  - controlcode = EVENT_TRACE_CONTROL_STOP。
    // 调用期间 name_wide 与 buf 均在栈上存活,指针有效;函数不会保存这些指针。
    let status = unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name_wide.as_ptr(),
            &mut buf.props as *mut EVENT_TRACE_PROPERTIES,
            EVENT_TRACE_CONTROL_STOP,
        )
    };

    match status {
        ERROR_SUCCESS => {
            tracing::debug!("已停止残留 ETW session: {name}");
        }
        ERROR_WMI_INSTANCE_NOT_FOUND => {
            // 本就不存在,无需清理。属正常路径。
            tracing::debug!("无残留 ETW session: {name}");
        }
        other => {
            // 其它错误(权限等)best-effort 忽略,绝不阻断后续 start。
            tracing::debug!("停止残留 ETW session {name} 返回码 {other}(忽略,继续启动)");
        }
    }
}

/// 判断 ferrisetw 的 `start()` 错误是否为 `ERROR_ALREADY_EXISTS`(同名 session 已存在)。
/// 仅此情形才值得「再清一次 + retry」(DESIGN §15.2 双保险)。
fn is_already_exists(err: &ferrisetw::trace::TraceError) -> bool {
    use ferrisetw::native::EvntraceNativeError;
    use ferrisetw::trace::TraceError;
    matches!(
        err,
        TraceError::EtwNativeError(EvntraceNativeError::AlreadyExist)
    )
}

impl NetMonitor for WinNetMonitor {
    fn start(
        &mut self,
        claude_pids: Arc<RwLock<HashSet<u32>>>,
        tx: Sender<NetEvent>,
    ) -> anyhow::Result<()> {
        // —— 启动自愈(DESIGN §15.2,修复 B):无条件先停同名残留 session,再建 ——
        // ETW realtime session 不随进程消亡;上次被强杀 / 未跑 Drop 时会残留,导致同名
        // StartTraceW 报 ERROR_ALREADY_EXISTS。这里先清后建,彻底自愈。best-effort,不阻断。
        stop_stale_trace(TRACE_NAME);

        // 构建 (trace, handle) 的闭包:因为回调闭包会消费 claude_pids/tx,而双保险 retry
        // 需要再建一次 trace,故把「克隆共享态 → 建 provider → start」整体封进闭包按需调用。
        // 闭包按 move 捕获 claude_pids/tx 的所有权,每次调用内部再 clone,从而可被多次调用(retry)。
        let build_trace = move || {
            // 每次构建都克隆共享态,使闭包可被多次调用(retry 用)。
            let claude_pids = claude_pids.clone();
            let tx = tx.clone();
            // 回调闭包:捕获共享的 claude_pids 与 tx。FnMut + Send + Sync + 'static 由 add_callback 要求。
            let provider = Provider::by_guid(KERNEL_NETWORK_PROVIDER_GUID)
                // 只订阅 IPv4+IPv6 流量 keyword(见 KERNEL_NET_KEYWORDS),收窄降噪。
                .any(KERNEL_NET_KEYWORDS)
                .add_callback(
                    move |record: &EventRecord, schema_locator: &SchemaLocator| {
                        on_event(record, schema_locator, &claude_pids, &tx);
                    },
                )
                .build();

            // 在「当前线程」同步 start():会调用 StartTraceW/EnableTraceEx2/OpenTraceW,
            // 权限不足 / provider 不可用会在此返回 Err,从而让上层据此降级为两态(DESIGN §9.3 步骤 1)。
            UserTrace::new()
                .named(TRACE_NAME.to_string())
                .enable(provider)
                .start()
        };

        // 首次尝试;若仍撞上 ERROR_ALREADY_EXISTS(stop 与 start 之间存在竞态,或残留 stop
        // 尚未完成),再 stop 一次后 retry 一次(DESIGN §15.2 双保险)。仍失败才返回 Err 降级。
        // 成功后拿到 (trace, handle):把 trace 移入后台线程持有(drop 即 stop),
        // 在后台线程上跑阻塞的 process 循环。
        let (trace, handle) = match build_trace() {
            Ok(ok) => ok,
            Err(e) if is_already_exists(&e) => {
                tracing::debug!("ETW start 撞上 ALREADY_EXISTS,再清一次残留后重试");
                stop_stale_trace(TRACE_NAME);
                build_trace()
                    .map_err(|e| anyhow::anyhow!("ETW trace 启动失败(重试后仍失败): {:?}", e))?
            }
            Err(e) => {
                return Err(anyhow::anyhow!("ETW trace 启动失败(需管理员?): {:?}", e));
            }
        };

        // 后台线程:持有 trace 保证 session 存活,process_from_handle 阻塞直到 trace 停止/进程退出。
        std::thread::Builder::new()
            .name("seecn-etw".to_string())
            .spawn(move || {
                // process_from_handle 阻塞运行回调循环;返回(出错或被停止)后让 trace 自然 drop -> stop。
                if let Err(e) = UserTrace::process_from_handle(handle) {
                    tracing::warn!("ETW process 循环退出: {:?}", e);
                }
                // 显式 keep-alive:确保 trace 在 process 循环结束前不被提前 drop。
                drop(trace);
            })
            .map_err(|e| anyhow::anyhow!("无法创建 ETW 处理线程: {e}"))?;

        Ok(())
    }
}

/// 回调主体。**热路径**:先按 pid 过滤,再解析、组事件、发送。
///
/// 约束(DESIGN §9.3):禁止 panic、禁止阻塞;所有解析失败一律 early-return。
fn on_event(
    record: &EventRecord,
    schema_locator: &SchemaLocator,
    claude_pids: &Arc<RwLock<HashSet<u32>>>,
    tx: &Sender<NetEvent>,
) {
    let event_id = record.event_id();

    // 只关心 TCP 的 Send/Recv/Connect/Disconnect/Retransmit;其余直接放过,避免无谓解析。
    if !is_interesting(event_id) {
        return;
    }

    // —— 先取 pid 并过滤(热路径要轻)——
    // 事件头自带的 ProcessId 在 KernelNetwork 上通常即「发起网络操作的进程」;
    // 若头里取不到(为 0 或无效),回退到 schema 的 PID 字段。
    let header_pid = record.process_id();

    // 解析 schema(SchemaLocator 内部缓存,命中后很廉价)。
    let schema = match schema_locator.event_schema(record) {
        Ok(s) => s,
        Err(_) => return, // 无 schema 无法解析字段,放弃
    };
    let parser = Parser::create(record, &schema);

    // 真正用于判定的 pid:优先 schema 的 PID 字段(更贴近网络操作的归属进程),
    // 取不到则用事件头 ProcessId。两者都拿不到就放弃。
    let pid = parser
        .try_parse::<u32>("PID")
        .ok()
        .filter(|p| *p != 0)
        .unwrap_or(header_pid);
    if pid == 0 {
        return;
    }

    // —— claude_pids 过滤:不关心的进程立即 return,绝不进入后续解析/发送 ——
    {
        let guard = match claude_pids.read() {
            Ok(g) => g,
            Err(_) => return, // 锁中毒:保守放弃本次事件,绝不 panic
        };
        if !guard.contains(&pid) {
            return;
        }
    } // 尽早释放读锁

    let is_v6 = is_ipv6(event_id);

    // 组装 ConnKey:local = saddr:sport(本机视角),remote = daddr:dport。
    // 解析尽力而为,失败兜底为 0,保证事件仍能投递(三态优先)。
    let key = build_conn_key(&parser, is_v6);

    // 按事件类型分派为 NetEvent。
    let event = match classify(event_id) {
        Kind::Send => {
            let size = parse_size(&parser);
            // outbound:只在 size>0 时才有意义;size=0 也照发,last_activity 会刷新(算一次活跃)。
            NetEvent::Data {
                pid,
                key,
                inbound: 0,
                outbound: size,
            }
        }
        Kind::Recv => {
            let size = parse_size(&parser);
            NetEvent::Data {
                pid,
                key,
                inbound: size,
                outbound: 0,
            }
        }
        Kind::Connect => NetEvent::Connect { pid, key },
        Kind::Disconnect => NetEvent::Disconnect { pid, key },
    };

    // send 失败说明接收端(Engine)已关闭,意味着应当退出;回调里忽略该错误即可。
    let _ = tx.send(event);
}

/// 事件分类(把多个 ID 归并成四种语义)。
enum Kind {
    Send,
    Recv,
    Connect,
    Disconnect,
}

/// 是否是我们关心的事件 ID。
fn is_interesting(event_id: u16) -> bool {
    matches!(
        event_id,
        EID_SEND_V4
            | EID_SEND_V6
            | EID_RECV_V4
            | EID_RECV_V6
            | EID_CONNECT_V4
            | EID_CONNECT_V6
            | EID_DISCONNECT_V4
            | EID_DISCONNECT_V6
            | EID_RETRANSMIT_V4
            | EID_RETRANSMIT_V6
    )
}

/// 该事件 ID 是否为 IPv6 变体(影响地址解析方式)。
fn is_ipv6(event_id: u16) -> bool {
    matches!(
        event_id,
        EID_SEND_V6 | EID_RECV_V6 | EID_CONNECT_V6 | EID_DISCONNECT_V6 | EID_RETRANSMIT_V6
    )
}

/// 把事件 ID 归类为四种语义。Retransmit 并入 Send(outbound),用于刷新活跃度。
fn classify(event_id: u16) -> Kind {
    match event_id {
        EID_SEND_V4 | EID_SEND_V6 | EID_RETRANSMIT_V4 | EID_RETRANSMIT_V6 => Kind::Send,
        EID_RECV_V4 | EID_RECV_V6 => Kind::Recv,
        EID_CONNECT_V4 | EID_CONNECT_V6 => Kind::Connect,
        // 其余(本函数只在 is_interesting 之后调用,剩下的就是 Disconnect)
        _ => Kind::Disconnect,
    }
}

/// 解析字节数 `size`(win:UInt32)。Connect/Disconnect 事件可能没有此字段,失败即 0。
fn parse_size(parser: &Parser) -> u64 {
    parser.try_parse::<u32>("size").map_or(0, u64::from)
}

/// 组装 ConnKey(local = saddr:sport,remote = daddr:dport)。
///
/// 字段解析全部尽力而为:解不到的部分兜底为 `0.0.0.0:0`(或 IPv6 `[::]:0`),
/// 保证事件仍可投递。Engine 用 (pid, key) 索引,即便 key 退化也能维持「连接存在性 + 活跃度」。
fn build_conn_key(parser: &Parser, is_v6: bool) -> ConnKey {
    let saddr = parse_addr(parser, "saddr", is_v6);
    let daddr = parse_addr(parser, "daddr", is_v6);
    let sport = parse_port(parser, "sport");
    let dport = parse_port(parser, "dport");
    ConnKey {
        local: SocketAddr::new(saddr, sport),
        remote: SocketAddr::new(daddr, dport),
    }
}

/// 解析地址字段。
///
/// KernelNetwork 不同 Windows 版本的 schema 形态不一:
///   * 有的把 daddr/saddr 标成带 OutTypeIpv4/Ipv6 → 可直接 `try_parse::<IpAddr>`;
///   * 经典 manifest 则是 IPv4=win:UInt32 / IPv6=win:Binary → 需手动转。
///
/// 因此「先按 IpAddr 试,失败再按原始类型试」,以 schema 实际为准(DESIGN §9.3)。
fn parse_addr(parser: &Parser, name: &str, is_v6: bool) -> IpAddr {
    // 1) 优先尝试 ferrisetw 内建的 IpAddr 解析(要求 OutTypeIpv4/Ipv6)。
    if let Ok(ip) = parser.try_parse::<IpAddr>(name) {
        return ip;
    }

    if is_v6 {
        // 2a) IPv6:win:Binary,16 字节,直接按网络序构造 Ipv6Addr。
        if let Ok(bytes) = parser.try_parse::<Vec<u8>>(name) {
            if bytes.len() == 16 {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&bytes[..16]);
                return IpAddr::V6(Ipv6Addr::from(octets));
            }
        }
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    } else {
        // 2b) IPv4:win:UInt32。try_parse::<u32> 用 from_ne_bytes,即按本机字节序读出 4 字节。
        //     IPv4 地址在 buffer 里以网络序(大端)的 a.b.c.d 排列;Ipv4Addr::from(u32) 期望
        //     大端 u32。因此把读出的 u32 的原始字节(本机序)还原后用 from_ne_bytes→to_ne_bytes
        //     的等价方式:直接用 u32 的内存字节构造地址,避免大小端误判。
        if let Ok(raw) = parser.try_parse::<u32>(name) {
            // raw 由 from_ne_bytes 得到:其内存字节序与 buffer 中一致(网络序 a.b.c.d)。
            // 取回这 4 个字节(本机序表示),即为 [a,b,c,d]。
            let octets = raw.to_ne_bytes();
            return IpAddr::V4(Ipv4Addr::from(octets));
        }
        // 兜底:也可能被标成 Binary。
        if let Ok(bytes) = parser.try_parse::<Vec<u8>>(name) {
            if bytes.len() >= 4 {
                return IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]));
            }
        }
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    }
}

/// 解析端口字段(win:UInt16)。
///
/// KernelNetwork 的 sport/dport 以**网络序(大端)**存储。try_parse::<u16> 走 from_ne_bytes,
/// 在小端机器上会读成主机序的「字节交换」值,因此需要 from_be 还原为真实端口号。
/// 若 schema 已带 OutTypePort 由 TDH 归一,则可能本就是主机序;此处统一做一次 swap_bytes 的
/// 等价处理:把 try_parse 得到的「本机序整数」当作大端原始字节解读。
fn parse_port(parser: &Parser, name: &str) -> u16 {
    match parser.try_parse::<u16>(name) {
        // try_parse 用 from_ne_bytes:在小端机上等于把大端原始字节读反了。
        // 取其原始字节(to_ne_bytes)再 from_be_bytes,得到真实端口(网络序→主机序)。
        Ok(v) => u16::from_be_bytes(v.to_ne_bytes()),
        Err(_) => 0,
    }
}
