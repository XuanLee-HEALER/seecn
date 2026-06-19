//! `WinTcpSnapshot`:基于 netstat2 的 TCP 表快照(DESIGN §9.2)。

use crate::model::ConnKey;
use crate::platform::TcpSnapshot;
use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use std::collections::HashSet;
use std::net::SocketAddr;

/// Windows TCP 连接快照器。仅用于「补已存在连接」,只关心 Established。
pub struct WinTcpSnapshot {}

impl WinTcpSnapshot {
    /// 构造快照器。
    pub fn new() -> Self {
        Self {}
    }
}

impl TcpSnapshot for WinTcpSnapshot {
    fn snapshot(&self, pids: &HashSet<u32>) -> Vec<(u32, ConnKey)> {
        // get_sockets_info(IPV4|IPV6, TCP),对每个 socket 取 associated_pids 与 pids 求交,
        // 仅保留 Established 的 TCP socket,组装 ConnKey 返回。
        let af_flags = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
        let proto_flags = ProtocolFlags::TCP;

        // 取不到快照(权限/系统异常)时,优雅降级为空向量,绝不 panic。
        let sockets = match get_sockets_info(af_flags, proto_flags) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("get_sockets_info 失败: {e}");
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for si in sockets {
            // 只处理 TCP;UDP 在本场景不会出现(已用 ProtocolFlags::TCP 过滤),稳妥起见仍解构判断。
            let tcp = match si.protocol_socket_info {
                ProtocolSocketInfo::Tcp(tcp) => tcp,
                ProtocolSocketInfo::Udp(_) => continue,
            };

            // 仅保留已建立连接(补连接只关心 Established)。
            if tcp.state != TcpState::Established {
                continue;
            }

            // local_addr / remote_addr 已是 IpAddr,端口为 u16,直接组装 SocketAddr。
            let key = ConnKey {
                local: SocketAddr::new(tcp.local_addr, tcp.local_port),
                remote: SocketAddr::new(tcp.remote_addr, tcp.remote_port),
            };

            // 一个 socket 可能关联多个 pid(共享句柄),对每个与关心集合相交的 pid 各产出一条。
            for &pid in &si.associated_pids {
                if pids.contains(&pid) {
                    out.push((pid, key));
                }
            }
        }

        out
    }
}
