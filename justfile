set shell := ["pwsh", "-NoLogo", "-NoProfile", "-Command"]

# 列出所有任务
default:
    @just --list

# 编译检查
check:
    cargo check

build:
    cargo build

release:
    cargo build --release

# 直接运行(非管理员,ETW 会降级为两态);带 seecn=debug 日志便于查错
run:
    $env:RUST_LOG='seecn=debug,info'; cargo run

# 以管理员身份运行(ETW 三态需要)——会弹 UAC;带 seecn=debug 日志便于查错
run-admin:
    Start-Process -Verb RunAs -FilePath pwsh -ArgumentList '-NoExit','-NoProfile','-Command','cd "{{justfile_directory()}}"; just run'

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets -- -D warnings

# 全量本地校验
ci: fmt check lint
    cargo build

# 安装为登录自启(管理员;无控制台 + 文件日志 + 最高权限,保留托盘/浮层)。
# 先 build release,再提权运行 scripts/install.ps1 注册计划任务(弹一次 UAC)。
install:
    cargo build --release
    Start-Process -Verb RunAs -FilePath pwsh -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"{{justfile_directory()}}\scripts\install.ps1"

# 卸载:移除计划任务并停止运行实例(管理员;弹一次 UAC)。
uninstall:
    Start-Process -Verb RunAs -FilePath pwsh -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',"{{justfile_directory()}}\scripts\uninstall.ps1"
