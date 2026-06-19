//! `WinProcScanner`:基于 sysinfo 的进程发现 + 命令行匹配(DESIGN §9.1)。

use crate::model::ClaudeProc;
use crate::platform::ProcScanner;
use sysinfo::{ProcessesToUpdate, System};

/// 命中规则的关键词(大小写不敏感,匹配任一即视为 Claude Code CLI)。
/// 集中为常量便于后续调参(DESIGN §9.1)。
///
/// - `claude-code`:典型的 npm 包路径(`...\@anthropic-ai\claude-code\cli.js`)。
/// - `claude.exe`:新版原生可执行文件出现在命令行里时。
/// - `\claude\`:命令行里出现 `\claude\` 目录段的兜底匹配。
pub const CLAUDE_MARKERS: &[&str] = &["claude-code", "claude.exe", r"\claude\"];

/// Electron 辅助进程命令行特征(`--type=renderer` / `gpu-process` / `utility` …)。
/// 通用 Electron 多进程标记,与版本/路径无关,覆盖 Claude Desktop 全部 helper(DESIGN §16.2)。
const ELECTRON_TYPE_MARKER: &str = "--type=";

/// Claude Desktop 的安装位标记(用于排除 Desktop;CLI 在用户 bin/node_modules,不沾这些)。
/// 入参为已小写化的 exe 路径,故标记本身也用小写(DESIGN §16.2):
/// - `windowsapps\claude`:MSIX/Store 安装(`C:\Program Files\WindowsApps\Claude_…`)。
/// - `anthropicclaude`:旧版 Squirrel 安装(`%LocalAppData%\AnthropicClaude\…`)。
const DESKTOP_DENY_PATH_MARKERS: &[&str] = &[r"windowsapps\claude", "anthropicclaude"];

/// 前置条件:进程名(小写,去掉 `.exe`)必须命中其一才进入命令行匹配。
///
/// 目的是宁缺毋滥(DESIGN §9.1 step 4 / §11.4):Claude Code CLI 在 Windows 上
/// 要么是 `node` 跑 `cli.js`,要么是原生 `claude.exe`;限定可执行名能避免把
/// 编辑器、本项目源码里偶然包含 `claude` 路径的进程误判为会话。
const ALLOWED_PROC_NAMES: &[&str] = &["node", "claude"];

/// Windows 进程扫描器。持有一个复用的 `System` 以减少分配。
pub struct WinProcScanner {
    sys: System,
}

impl WinProcScanner {
    /// 构造扫描器,初始化复用的 `System`。
    pub fn new() -> Self {
        Self { sys: System::new() }
    }
}

impl Default for WinProcScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// 判断进程名(已小写)是否满足前置条件(`node` / `claude`,带不带 `.exe` 均可)。
fn name_allowed(name_lower: &str) -> bool {
    let stem = name_lower.strip_suffix(".exe").unwrap_or(name_lower);
    ALLOWED_PROC_NAMES.contains(&stem)
}

/// 判断命令行(整体小写串)是否命中任一 Claude 标记。
fn cmd_hits(cmd_lower: &str) -> bool {
    CLAUDE_MARKERS.iter().any(|m| cmd_lower.contains(m))
}

/// 是否为 Claude Desktop / Electron 进程(应被排除,DESIGN §16.2)。入参均为小写。
///
/// 命中任一即判定为「非 CLI」:
/// 1. 命令行含 `--type=`:Electron 辅助进程(crashpad/gpu/renderer/utility …)。
/// 2. exe 路径含任一 Desktop 安装标记:覆盖没有 `--type=` 的 Desktop 主进程。
fn is_desktop_or_electron(exe_path_lower: &str, cmd_lower: &str) -> bool {
    cmd_lower.contains(ELECTRON_TYPE_MARKER)
        || DESKTOP_DENY_PATH_MARKERS
            .iter()
            .any(|m| exe_path_lower.contains(m))
}

impl ProcScanner for WinProcScanner {
    fn scan(&mut self) -> Vec<ClaudeProc> {
        // 1. 刷新全量进程表(同时移除已消失的进程)。API 以 sysinfo 0.39 为准。
        self.sys.refresh_processes(ProcessesToUpdate::All, true);

        let self_pid = std::process::id();
        let mut out = Vec::new();

        // 2. 遍历进程,按 §9.1 规则匹配。
        for proc in self.sys.processes().values() {
            let pid = proc.pid().as_u32();

            // 排除自身,避免本进程命令行里的 "claude" 字样自我命中(§9.1 step 4)。
            if pid == self_pid {
                continue;
            }

            // 进程名:&OsStr → 有损 String → 小写。
            let name_lower = proc.name().to_string_lossy().to_ascii_lowercase();

            // cmd():&[OsString];整体拼成一个小写串再做子串匹配。
            // 单段命中即整体命中,故拼接后匹配等价且更省分配。
            let cmd_lower = proc
                .cmd()
                .iter()
                .map(|seg| seg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();

            // exe() 在 sysinfo 0.39 返回 Option<&Path>;取不到按空串处理(§16.2):
            // 空路径不含 Desktop 标记 → 不误伤;同时 `--type=` 那条仍能据 cmd 排除。
            let exe_path_lower = proc
                .exe()
                .map(|p| p.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();

            // deny 闸(§16.2):正向命中之前先排除 Claude Desktop / Electron。
            // Desktop 的辅助进程同名 claude.exe,只看名字会被误纳(§16.1)。
            if is_desktop_or_electron(&exe_path_lower, &cmd_lower) {
                tracing::debug!(
                    pid,
                    path = %exe_path_lower,
                    reason = "Electron/Desktop",
                    "排除非 CLI 进程"
                );
                continue;
            }

            // 命中规则(大小写不敏感,匹配任一):
            //   a) 进程名为 claude.exe;或
            //   b) 满足前置条件(node/claude.exe)且命令行任意处含 Claude 标记
            //      (覆盖 `node cli.js` 形态)。
            // 前置条件是保守过滤,宁缺毋滥(§9.1 step 4 / §11.4)。
            let name_is_claude_exe = name_lower == "claude.exe";

            let hit = name_is_claude_exe || (name_allowed(&name_lower) && cmd_hits(&cmd_lower));

            if !hit {
                continue;
            }

            // cmdline 用于 tooltip 展示与调试(§7 Session/ClaudeProc 契约)。
            // 用原始大小写(非小写化的匹配串)还原命令行,展示更可读。
            let cmdline = proc
                .cmd()
                .iter()
                .map(|seg| seg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");

            // 命中进程记 debug 日志,便于实测校准(§11.4)。
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
    fn desktop_msix_path_is_excluded() {
        // Desktop MSIX/Store 安装:路径含 windowsapps\claude,无 --type= 也应排除。
        let exe = r"c:\program files\windowsapps\claude_1.2.3_x64__abc\app\claude.exe";
        assert!(is_desktop_or_electron(exe, "claude.exe"));
    }

    #[test]
    fn electron_renderer_cmd_is_excluded() {
        // Electron 辅助进程:命令行含 --type=renderer。
        let cmd = r"claude.exe --type=renderer --enable-features=foo";
        assert!(is_desktop_or_electron("", cmd));
    }

    #[test]
    fn cli_local_bin_is_not_excluded() {
        // CLI 原生安装位 \.local\bin\claude.exe 且无 --type= → 不应被排除。
        let exe = r"c:\users\lixuan\.local\bin\claude.exe";
        assert!(!is_desktop_or_electron(exe, "claude.exe"));
    }

    #[test]
    fn anthropicclaude_path_is_excluded() {
        // 旧版 Squirrel 安装:%LocalAppData%\AnthropicClaude\…。
        let exe = r"c:\users\lixuan\appdata\local\anthropicclaude\app-0.1.0\claude.exe";
        assert!(is_desktop_or_electron(exe, "claude.exe"));
    }
}
