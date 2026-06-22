//! `MacTcpSnapshot`:基于 netstat2 的 TCP 表快照(对应 windows/tcptable.rs)。
//!
//! 仅用于「补已存在连接」(net monitor 只在有流量时才报告连接,空闲连接靠本快照补全
//! 连接存在性,从而支撑 Idle 态)。只关心 Established。netstat2 跨平台,macOS 同样可用,
//! 非 root 即可拿到本用户进程的 socket。

use crate::model::ConnKey;
use crate::platform::TcpSnapshot;
use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use std::collections::HashSet;
use std::net::SocketAddr;

/// macOS TCP 连接快照器。
pub struct MacTcpSnapshot {}

impl MacTcpSnapshot {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for MacTcpSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpSnapshot for MacTcpSnapshot {
    fn snapshot(&self, pids: &HashSet<u32>) -> Vec<(u32, ConnKey)> {
        let af_flags = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
        let proto_flags = ProtocolFlags::TCP;

        // 取不到快照(权限/系统异常)时优雅降级为空,绝不 panic。
        let sockets = match get_sockets_info(af_flags, proto_flags) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("get_sockets_info 失败: {e}");
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for si in sockets {
            let tcp = match si.protocol_socket_info {
                ProtocolSocketInfo::Tcp(tcp) => tcp,
                ProtocolSocketInfo::Udp(_) => continue,
            };

            // 补连接只关心已建立连接。
            if tcp.state != TcpState::Established {
                continue;
            }

            let key = ConnKey {
                local: SocketAddr::new(tcp.local_addr, tcp.local_port),
                remote: SocketAddr::new(tcp.remote_addr, tcp.remote_port),
            };

            // 一个 socket 可能关联多个 pid;对与关心集合相交的每个 pid 各产出一条。
            for &pid in &si.associated_pids {
                if pids.contains(&pid) {
                    out.push((pid, key));
                }
            }
        }

        out
    }
}
