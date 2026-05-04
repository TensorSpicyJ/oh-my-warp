# omw-windows-build — Implementation Plan (2-Person)

> **v0.0.0 scope:** 编译 + 审计 + 打包。omw-server / omw-agent / research-agents 延后。
>
> **上手：** 两人各自读完 Prerequisites，然后 A 主线编译，B 配套脚本——两条线独立启动，交汇点在 A 出二进制之后。

**Goal:** 从本 repo（Warp fork）编译出审计干净的 Windows .exe——编译期裁掉所有 warp.dev / Firebase / Oz 云端端点。

**Repo:** https://github.com/Taishaojie061/omw-windows
**Tech Stack:** Rust 1.92+, Cargo, Git, Windows SDK, PowerShell

---

## 分工总览

```
A 主线编译                          B 配套脚本
─────────────                      ──────────────
A-1 clone + 环境检查               B-1 clone + 环境检查（各自做）
  │                                  │
A-2 验证上游编译 (40-60min)         B-2 写 audit-no-cloud.ps1
  │                                  │
A-3 改 5 文件 + 加 omw_local       B-3 写 package.ps1
  │                                  │
A-4 omw_local 编译 (40-60min)       B-4 准备冒烟测试 checklist
  │                                  │
  └──────── 交汇 ────────┘
              │
         跑 audit-no-cloud.ps1
              │
         打包 .zip
              │
         冒烟测试
```

A 的两次编译各 40-60 分钟——B 在这期间完全不受阻塞。

---

## Prerequisites（两人各自检查）

```powershell
# 1. Rust toolchain (>= 1.92)
rustup show
# Expected: stable-x86_64-pc-windows-msvc

# 2. protoc (>= 3.x)
protoc --version
# If missing: winget install --id Google.Protobuf
# Then restart your terminal and verify PATH includes protoc

# 3. Git
git --version
```

---

## Track A — 主线编译

### A-1: Clone

```powershell
git clone https://github.com/Taishaojie061/omw-windows.git
cd omw-windows
git checkout omw-windows
```

本 repo 是 warpdotdev/warp 的 fork，源码在根目录，没有 vendor 层。

### A-2: 验证上游编译（不改代码）

这一步用原版 Warp 编译，证明工具链能跑通。**不要改任何文件。**

```powershell
cd omw-windows
cargo build --release -p warp --bin warp-oss 2>&1 | tee build-upstream.log
```

耗时约 40-60 分钟。如果失败，常见原因：
- 缺 Windows SDK → 装 Visual Studio Build Tools（勾选 C++ workload + Windows 11 SDK）
- 缺 protoc → `winget install --id Google.Protobuf`
- conpty/DXC DLL 找不到 → 检查 `app/assets/windows/x64/` 下是否有 conpty.dll 等

编译完成后验证：
```powershell
ls target/release/warp-oss.exe               # 文件应存在，150-300 MB
.\target\release\warp-oss.exe --version 2>&1  # 至少不报 DLL 错误
```

### A-3: 改 5 个文件 + 定义 omw_local feature

核心思路：定义一个 `omw_local` feature，用 `#[cfg(feature = "omw_local")]` 在编译期把云端 URL 替换为 localhost/空值。

**文件 1: `app/Cargo.toml` — 添加 omw_local feature**

找到 `[features]` 段落。warp 原版没有 `omw_local`，需要新增。先读出当前 `[features]` 段，在 `default` 下方添加 `omw_local`：

```toml
[features]
default     = ["warp_core/cloud", "warp-command-signatures/embed-signatures"]
omw_local   = ["warp_core/omw_local"]
# ... 其他已有的 feature 不动 ...
```

注意：
- `omw_local` **不**包含 `warp_core/cloud` 和 `warp-command-signatures/embed-signatures`
- `omw_local` 需要传导 `warp_core/omw_local` 到下层 crate

如果 `[features]` 段里没有 `default` 或 `cloud` 的显式定义，就去 `warp_core/Cargo.toml` 看当前的 feature 结构，然后对齐。

**文件 2: `crates/warp_core/Cargo.toml` — 添加 omw_local feature**

检查 `[features]` 段，确认有 `cloud` feature（或类似的云端开关），然后添加：

```toml
[features]
cloud = []
omw_local = []
```

这两个 feature 互斥——编译时只开其中一个。

**文件 3: `crates/warp_completer/Cargo.toml` — 去掉无条件的 embed-signatures**

```toml
# 找到（约第 47 行）：
warp-command-signatures = {workspace = true, features = ["embed-signatures"]}

# 改为：
warp-command-signatures = {workspace = true}
```

embed-signatures 现在由 `app/Cargo.toml` 的 feature 控制（`omw_local` 不包含它，即编译时不会嵌入云端签名）。

**文件 4: `crates/warp_core/src/channel/config.rs` — cfg-gate 云端 URL**

当前 `production()` 返回硬编码的 warp.dev 地址。用双份实现做编译期裁切：

```rust
impl WarpServerConfig {
    #[cfg(not(feature = "omw_local"))]
    pub fn production() -> Self {
        Self {
            server_root_url: "https://app.warp.dev".into(),
            rtc_server_url: "wss://rtc.app.warp.dev/graphql/v2".into(),
            session_sharing_server_url: Some("wss://sessions.app.warp.dev".into()),
            firebase_auth_api_key: "AIzaSyBdy3O3S9hrdayLJxJ7mriBR4qgUaUygAs".into(),
        }
    }

    #[cfg(feature = "omw_local")]
    pub fn production() -> Self {
        Self {
            server_root_url: "http://127.0.0.1:0".into(),
            rtc_server_url: "ws://127.0.0.1:0".into(),
            session_sharing_server_url: None,
            firebase_auth_api_key: String::new().into(),
        }
    }
}

impl OzConfig {
    #[cfg(not(feature = "omw_local"))]
    pub fn production() -> Self {
        Self {
            oz_root_url: "https://oz.warp.dev".into(),
            workload_audience_url: None,
        }
    }

    #[cfg(feature = "omw_local")]
    pub fn production() -> Self {
        Self {
            oz_root_url: "http://127.0.0.1:0".into(),
            workload_audience_url: None,
        }
    }
}
```

**文件 5: `app/src/auth/credentials.rs` — cfg-gate Firebase 端点**

当前 `access_token_url()` 动态拼 URL。在 `omw_local` 下返回空字符串：

```rust
impl FirebaseToken {
    #[cfg(not(feature = "omw_local"))]
    pub fn access_token_url(&self, api_key: &str) -> String {
        match self {
            FirebaseToken::Refresh(_) => {
                format!("https://securetoken.googleapis.com/v1/token?key={api_key}")
            }
            FirebaseToken::Custom(_) => {
                format!("https://identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken?key={api_key}")
            }
        }
    }

    #[cfg(feature = "omw_local")]
    pub fn access_token_url(&self, _api_key: &str) -> String {
        String::new()
    }
}
```

**文件 6（额外）: 检查其他可能残留 URL 的文件**

```powershell
# 搜索所有 Rust 和 sh 文件里有没有遗漏的云端域名
cd omw-windows
rg -l 'warp\.dev|firebase\.googleapis|identitytoolkit\.googleapis|securetoken\.googleapis' --type rust --type sh
```

对每个命中的文件，判断是否需要加 cfg-gate。常见遗漏：
- `crates/warp_core/src/channel/config.rs` 里的 `RudderStackConfig`（telemetry，如果 `omw_local` 下不初始化 telemetry 则无害）
- `crates/remote_server/src/install_remote_server.sh` 注释里的 URL（会被 `include_str!()` 嵌进二进制）→ 改为 `example.com`

改完提交：
```bash
git checkout -b omw/windows-local
git add -A
git commit -m "feat: add omw_local feature — cloud-strip for Windows build"
```

### A-4: 编译 omw_local 版

```powershell
cd omw-windows
cargo build --release -p warp --bin warp-oss --no-default-features --features omw_local 2>&1 | tee build-omw-local.log
```

耗时约 40-60 分钟。首次编译后如果想加速增量构建，建议检查 `Cargo.toml` 里的 `[profile.release]` 设置（lto、codegen-units 等影响速度）。

完成后：
```powershell
ls target/release/warp-oss.exe              # 确认存在
dumpbin /dependents target/release/warp-oss.exe 2>&1 | Select-Object -First 30
# 应该只依赖 conpty.dll, dxcompiler.dll, dxil.dll 等
```

> **到这里给 B 发信号：** "二进制出了，路径 target/release/warp-oss.exe，可以跑审计。"

---

## Track B — 配套脚本

B 的三个任务完全独立，不需要等 A 的编译结果——写完脚本等 A 给二进制路径即可验证。

### B-1: Clone

```powershell
git clone https://github.com/Taishaojie061/omw-windows.git
cd omw-windows
git checkout omw-windows
```

### B-2: 写审计脚本

在 repo 根目录创建 `scripts/audit-no-cloud.ps1`：

```powershell
# audit-no-cloud.ps1
# Usage: powershell -ExecutionPolicy Bypass -File scripts/audit-no-cloud.ps1 <path-to-warp-oss.exe>

param(
    [Parameter(Mandatory=$true)]
    [string]$Binary
)

$binary = Resolve-Path $Binary
if (-not (Test-Path $binary)) {
    Write-Host "ERROR: Binary not found: $binary" -ForegroundColor Red
    exit 1
}

$forbidden = @(
    "app.warp.dev",
    "api.warp.dev",
    "cloud.warp.dev",
    "oz.warp.dev",
    "rtc.app.warp.dev",
    "sessions.app.warp.dev",
    "firebase.googleapis.com",
    "firebaseio.com",
    "identitytoolkit.googleapis.com",
    "securetoken.googleapis.com"
)

Write-Host "Auditing: $binary`n"

# Try strings.exe first (ships with Git for Windows)
$strings = & "C:\Program Files\Git\usr\bin\strings.exe" $binary 2>$null
if (-not $strings) {
    Write-Host "strings.exe not found, using PowerShell fallback..." -ForegroundColor Yellow
    $bytes = [System.IO.File]::ReadAllBytes($binary)
    $sb = [System.Text.StringBuilder]::new()
    $run = [System.Collections.ArrayList]::new()
    foreach ($b in $bytes) {
        if ($b -ge 32 -and $b -le 126) {
            [void]$sb.Append([char]$b)
        } else {
            if ($sb.Length -ge 4) { [void]$run.Add($sb.ToString()) }
            [void]$sb.Clear()
        }
    }
    $strings = $run
}

$failures = 0
foreach ($hostname in $forbidden) {
    $hits = ($strings | Select-String -Pattern ([regex]::Escape($hostname))).Count
    if ($hits -gt 0) {
        Write-Host "FAIL: '$hostname' found $hits time(s)" -ForegroundColor Red
        $failures++
    } else {
        Write-Host "OK:   '$hostname' — zero hits" -ForegroundColor Green
    }
}

Write-Host "`n$failures / $($forbidden.Count) forbidden hostnames found."
if ($failures -gt 0) {
    Write-Host "AUDIT FAILED" -ForegroundColor Red
    Write-Host "`nTroubleshooting:"
    Write-Host "  1. grep source: rg '<hostname>' . --type rust --type sh"
    Write-Host "  2. Check for URLs in doc comments (include_str! embeds them)"
    Write-Host "  3. Re-check the cfg-gate files in Track A-3"
    exit 1
} else {
    Write-Host "AUDIT PASSED — binary is clean" -ForegroundColor Green
    exit 0
}
```

### B-3: 写打包脚本

在 repo 根目录创建 `scripts/package.ps1`：

```powershell
# package.ps1
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package.ps1 -Version "v0.0.0-windows"

param(
    [string]$Version = "v0.0.0-windows"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$bin = "$root\target\release\warp-oss.exe"
$staging = "$root\omw-warp-oss-$Version"
$out = "$root\omw-warp-oss-$Version-x86_64-windows.zip"

if (-not (Test-Path $bin)) {
    Write-Host "ERROR: Binary not found at $bin" -ForegroundColor Red
    Write-Host "Run Track A-4 first." -ForegroundColor Yellow
    exit 1
}

# Clean staging
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
New-Item -ItemType Directory -Path $staging -Force | Out-Null

# Copy binary
Copy-Item $bin "$staging\omw-warp-oss.exe"

# Copy runtime DLLs
$assets = "$root\app\assets\windows\x64"
if (Test-Path "$assets\conpty.dll") { Copy-Item "$assets\conpty.dll" $staging\ }
if (Test-Path "$assets\dxcompiler.dll") { Copy-Item "$assets\dxcompiler.dll" $staging\ }
if (Test-Path "$assets\dxil.dll") { Copy-Item "$assets\dxil.dll" $staging\ }
if (Test-Path "$assets\OpenConsole.exe") { Copy-Item "$assets\OpenConsole.exe" $staging\ }

# Copy LICENSE
$licensePath = "$root\LICENSE-AGPL"
if (-not (Test-Path $licensePath)) { $licensePath = "$root\LICENSE" }
if (Test-Path $licensePath) { Copy-Item $licensePath "$staging\LICENSE.txt" }

# Write README
@"
# omw-warp-oss $Version (Windows)

Cloud-stripped Warp terminal for Windows. All cloud endpoints removed at
compile time via `omw_local` feature. No sign-in required. No phoning home.

## Quick start

Run `omw-warp-oss.exe`. On first launch Windows SmartScreen may block this
unsigned build — click "More info" → "Run anyway".

## Audit

Audited against 10 forbidden hostnames (all zero hits):
app.warp.dev, api.warp.dev, cloud.warp.dev, oz.warp.dev,
rtc.app.warp.dev, sessions.app.warp.dev,
firebase.googleapis.com, firebaseio.com,
identitytoolkit.googleapis.com, securetoken.googleapis.com.

## Source

https://github.com/Taishaojie061/omw-windows
Built with: cargo build --release -p warp --bin warp-oss --no-default-features --features omw_local

## License

AGPL-3.0. See LICENSE.txt.
"@ | Out-File -Encoding UTF8 "$staging\README.md"

# Package
if (Test-Path $out) { Remove-Item $out }
Compress-Archive -Path "$staging\*" -DestinationPath $out

Write-Host "Packaged: $out" -ForegroundColor Green
Write-Host "Size: $([math]::Round((Get-Item $out).Length / 1MB, 1)) MB"
```

### B-4: 冒烟测试 checklist

A 出二进制后，按下面步骤验证：

```powershell
cd omw-windows

# Step 1: 确认运行时 DLL 齐
ls app/assets/windows/x64/conpty.dll
ls app/assets/windows/x64/dxcompiler.dll
ls app/assets/windows/x64/dxil.dll
ls app/assets/windows/x64/OpenConsole.exe

# Step 2: 把 DLL 拷到二进制旁边
Copy-Item app/assets/windows/x64/conpty.dll target/release/
Copy-Item app/assets/windows/x64/dxcompiler.dll target/release/
Copy-Item app/assets/windows/x64/dxil.dll target/release/
Copy-Item app/assets/windows/x64/OpenConsole.exe target/release/

# Step 3: 启动终端
.\target\release\warp-oss.exe
# Expected: 终端窗口打开，出现 shell 提示符
# 不应出现 "Sign in" 登录提示

# Step 4: 检查网络
# 打开 resmon.exe → Network 标签 → 过滤 warp-oss.exe
# 敲几条命令，Ctrl+P 打开命令面板
# 确认没有任何到 warp.dev 或 firebase 域名的外连

# Step 5: 检查日志
ls $env:USERPROFILE\Library\Logs\warp-oss.log
cat $env:USERPROFILE\Library\Logs\warp-oss.log | Select-Object -Last 50
# 只有启动诊断信息，无云端错误
```

---

## 交汇 — 两人一起

A 编译出二进制后：

| 步骤 | 谁做 | 做什么 |
|------|------|--------|
| 审计 | B 跑 | `scripts/audit-no-cloud.ps1 target/release/warp-oss.exe` → 10/10 PASS |
| 审计不通过 | A 修 | `rg` 搜源码找残留域名，补 cfg-gate |
| 打包 | B 跑 | `scripts/package.ps1` → 出 .zip |
| 冒烟测试 | 两人各自 | 启动终端、检查网络、检查日志 |
| 提交 | A | commit 审计 + 打包脚本到 `omw/windows-local` 分支 |

---

## Post-Plan: 不在 v0.0.0 范围

- **Code signing** — EV 证书，首次预览不做
- **MSI installer** — 便携 .zip 足够
- **Binary rename (warp-oss → omw)** — 以后做
- **omw-server + omw-agent** — oh-my-warp 的组件，本 repo 不涉及
- **Research agent 层** — 独立 plan
- **ARM64** — 先稳定 x64

---

## 参考

- 上游 Warp: https://github.com/warpdotdev/warp
- 上游 oh-my-warp: https://github.com/AndrewWayne/oh-my-warp
- Design spec: `docs/omw-windows/DESIGN.md`
