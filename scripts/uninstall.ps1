# seecn 卸载(需管理员)——DESIGN §22.3
#   移除计划任务并停止运行实例。安装目录 %LOCALAPPDATA%\seecn 及日志保留(如需彻底清除请手动删除)。
# 由 `just uninstall` 经 `Start-Process -Verb RunAs` 提权调用。
$ErrorActionPreference = 'SilentlyContinue'

$taskName   = 'seecn'
$installDir = Join-Path $env:LOCALAPPDATA 'seecn'

if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $taskName
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    Write-Host "[seecn] 已移除计划任务 '$taskName'"
} else {
    Write-Host "[seecn] 未发现计划任务 '$taskName'(可能未安装)"
}

Get-Process seecn -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Write-Host "[seecn] 已停止运行实例"
Write-Host "[seecn] 安装目录与日志保留:$installDir(如需彻底清除请手动删除)"
