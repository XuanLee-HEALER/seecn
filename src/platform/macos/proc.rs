//! `MacProcScanner`:基于 sysinfo 的进程发现 + CLI/Desktop 隔离(对应 windows/proc.rs)。
//!
//! macOS 实测(本机 Apple Silicon):CLI 即原生 `claude` 进程,`comm=="claude"`,
//! 命令行形如 `claude --dangerously-skip-permissions`,装在 `~/.local/share/claude/versions/`。
//! Desktop 的 claude code 走 Claude.app 的 Electron 多进程(带 `--type=` / 装在 app bundle)。

use crate::model::ClaudeProc;
use crate::platform::ProcScanner;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// node 形态(npm 安装)命令行标记。macOS 原生 CLI 直接靠 `comm=="claude"` 命中;
/// 这些标记仅用于覆盖 `node .../@anthropic-ai/claude-code/cli.js` 的兜底形态。
pub const CLAUDE_MARKERS: &[&str] = &["claude-code", "/claude/"];

/// Electron 辅助进程命令行特征(`--type=renderer` / `gpu-process` …)。
const ELECTRON_TYPE_MARKER: &str = "--type=";

/// Claude Desktop 安装位标记(小写 exe 路径子串)。macOS 上 Desktop 全部进程都在 app bundle 内。
const DESKTOP_DENY_PATH_MARKERS: &[&str] = &["/applications/claude.app/"];

/// 进程名前置条件:macOS CLI 即原生 `claude`;`node` 覆盖 npm 形态。
const ALLOWED_PROC_NAMES: &[&str] = &["node", "claude"];

/// macOS 进程扫描器。持有复用的 `System` 以减少分配。
pub struct MacProcScanner {
    sys: System,
}

impl MacProcScanner {
    pub fn new() -> Self {
        Self { sys: System::new() }
    }
}

impl Default for MacProcScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程名(已小写)是否满足前置条件。
fn name_allowed(name_lower: &str) -> bool {
    ALLOWED_PROC_NAMES.contains(&name_lower)
}

/// 命令行(整体小写)是否命中任一 Claude 标记。
fn cmd_hits(cmd_lower: &str) -> bool {
    CLAUDE_MARKERS.iter().any(|m| cmd_lower.contains(m))
}

/// 是否为 Claude Desktop / Electron 进程(应排除)。入参均为小写。
fn is_desktop_or_electron(exe_path_lower: &str, cmd_lower: &str) -> bool {
    cmd_lower.contains(ELECTRON_TYPE_MARKER)
        || DESKTOP_DENY_PATH_MARKERS
            .iter()
            .any(|m| exe_path_lower.contains(m))
}

impl ProcScanner for MacProcScanner {
    fn scan(&mut self) -> Vec<ClaudeProc> {
        // macOS 的默认 refresh 不拉 cmd/exe(实测 cmdline 为空)。显式要 cmd + exe:
        // cmd 喂 cmdline 展示 + node 形态匹配,exe 路径用于 Desktop/Electron 隔离。
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );

        let self_pid = std::process::id();
        let mut out = Vec::new();

        for proc in self.sys.processes().values() {
            let pid = proc.pid().as_u32();
            if pid == self_pid {
                continue;
            }

            let name_lower = proc.name().to_string_lossy().to_ascii_lowercase();
            let cmd_lower = proc
                .cmd()
                .iter()
                .map(|seg| seg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            let exe_path_lower = proc
                .exe()
                .map(|p| p.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();

            // deny 闸:先排除 Desktop / Electron,再做正向命中。
            if is_desktop_or_electron(&exe_path_lower, &cmd_lower) {
                tracing::debug!(pid, path = %exe_path_lower, reason = "Electron/Desktop", "排除非 CLI 进程");
                continue;
            }

            // 命中:a) 原生 `claude`;或 b) node/claude 且命令行含 Claude 标记。
            let name_is_claude = name_lower == "claude";
            let hit = name_is_claude || (name_allowed(&name_lower) && cmd_hits(&cmd_lower));
            if !hit {
                continue;
            }

            let cmdline = proc
                .cmd()
                .iter()
                .map(|seg| seg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            tracing::debug!(pid, name = %name_lower, cmdline = %cmdline, "命中 Claude 进程");
            out.push(ClaudeProc { pid, cmdline });
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 入参契约:exe 路径与命令行均为已小写化的串(与 scan() 调用点一致)。

    #[test]
    fn desktop_app_bundle_is_excluded() {
        // Desktop 主进程:exe 在 /Applications/Claude.app/ 内,即使无 --type= 也排除。
        let exe = "/applications/claude.app/contents/macos/claude";
        assert!(is_desktop_or_electron(exe, "/applications/claude.app/contents/macos/claude"));
    }

    #[test]
    fn electron_helper_cmd_is_excluded() {
        // Electron 辅助进程:命令行含 --type=renderer。
        let cmd = "/applications/claude.app/.../claude helper --type=renderer";
        assert!(is_desktop_or_electron("", cmd));
    }

    #[test]
    fn cli_local_share_is_not_excluded() {
        // CLI 原生安装位 ~/.local/share/claude/versions/.. 且无 --type= → 不排除。
        let exe = "/users/lixuan/.local/share/claude/versions/1.2.3/claude";
        assert!(!is_desktop_or_electron(exe, "claude --dangerously-skip-permissions"));
    }

    #[test]
    fn native_claude_name_hits() {
        // macOS 原生 CLI:comm=claude 直接命中,不依赖命令行标记。
        assert!(name_allowed("claude"));
    }
}
