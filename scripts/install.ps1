# seecn 安装为登录自启(需管理员)——DESIGN §22.3
#   1) 把 release 版 seecn.exe 拷到 %LOCALAPPDATA%\seecn\
#   2) 注册计划任务:登录触发 / 当前用户 / 最高权限(免每次 UAC,ETW 三态可用)
#   3) 停掉在跑实例,立即启动一份
# 由 `just install` 经 `Start-Process -Verb RunAs` 提权调用;也可在管理员 PowerShell 里直接跑。
$ErrorActionPreference = 'Stop'

$taskName   = 'seecn'
$installDir = Join-Path $env:LOCALAPPDATA 'seecn'
$logDir     = Join-Path $installDir 'logs'
$destExe    = Join-Path $installDir 'seecn.exe'
# release exe 在脚本上一级目录的 target\release 下
$srcExe     = Join-Path $PSScriptRoot '..\target\release\seecn.exe'

if (-not (Test-Path $srcExe)) {
    throw "找不到 release exe:$srcExe(请先 ``cargo build --release`` 或 ``just install`` 会自动构建)"
}

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
Copy-Item -Force -Path $srcExe -Destination $destExe
Write-Host "[seecn] 已拷贝 exe -> $destExe"

$user      = "$env:USERDOMAIN\$env:USERNAME"
$action    = New-ScheduledTaskAction -Execute $destExe
$trigger   = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
$settings  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
                -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null
Write-Host "[seecn] 已注册计划任务 '$taskName'(登录自启 + 最高权限)"

# 停掉可能在跑的实例(避免开发实例与任务实例共存),再立即启动一份
Get-Process seecn -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-ScheduledTask -TaskName $taskName
Write-Host "[seecn] 已在后台启动(无窗口);日志:$logDir\seecn.log"
Write-Host "[seecn] 完成。注销/重启后会自动启动。卸载:just uninstall"
