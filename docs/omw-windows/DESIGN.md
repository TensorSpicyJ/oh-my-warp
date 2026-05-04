# omw-windows-research-terminal — 设计规格

2026-05-02

## 版本范围

| 版本 | 范围 |
|------|------|
| **v0.0.0** | omw-warp-oss.exe 编译 + 审计 + 便携打包（Phase 1-4 of build plan） |
| v0.1+ | omw-server + omw-agent + BYOK provider 路由 |
| v0.3+ | Research-agent 层（文献/数据分析/理论/论文） |

> v0.0.0 仅交付一个可启动、零外连的 Windows 终端二进制。Research-agent 层不在本版本范围。

## 概述

基于 [warpdotdev/warp](https://github.com/warpdotdev/warp)（AGPL-3.0）fork，直接在上游源码上做编译期去云端，构建 Windows 版研究型终端。终端保持传统操作体验，通过内联 AI 对话 + agent 调度实现科研工作流深度定制。终端层与科研 agent 层独立演进。

## 路线选择

方案 C：分层演进。oh-my-warp 负责终端 + AI 交互框架，科研能力以独立 agent 层通过 ACP 协议接入。

---

## 1. 整体架构

```
omw-warp Windows (去云端 Warp fork)
  ├── Terminal (PTY)
  ├── AI Chat (内联对话)
  ├── Agent Panel (任务分发/监控)
  └── omw-server (axum, localhost)
        │
  ┌─────┼─────────────┐
  │     │             │
  v     v             v
omw-agent   provider   research-agents
(TypeScript  router     (Python/TS)
 pi-agent)  ┌────────┐  ├── lit-search
            │Claude   │  ├── data-analysis
            │DeepSeek │  ├── theory-physics
            │local    │  ├── training-pipe
            └────────┘  ├── feishu-delivery
                        └── paper-writing
```

分层职责：

| 层 | 角色 | 语言 | 定制程度 |
|---|------|------|---------|
| omw-warp GUI | 终端体验 + AI 交互窗口 | Rust | 最小（去云端 + 品牌） |
| omw-server | 本地后端，路由请求 | Rust | 保持原样 |
| omw-agent | AI 对话引擎，工具调度 | TypeScript | 扩展 provider 路由 |
| research-agents | 科研专用 agent | Python/TS | **核心定制层** |

---

## 2. Provider 路由

三类 provider：

| Provider | 用途 | 运行位置 |
|----------|------|---------|
| Claude API / DeepSeek API | 日常对话、代码、写作 | 云端，自有 key |
| 本地推理 endpoint | 实验数据训练后的模型推理 | 本地 GPU（后续） |
| 本地 embedding / 小模型 | 文献检索、快速分类 | 本地 CPU |

路由规则：
- 终端内 ai 对话默认走 Claude API，可通过 flag 切换
- 本地模型按模型名自动路由到对应 endpoint
- 本地模型不可用时降级到云 API（带提示）

安全约束：
- API key 存本地 OS keychain（omw-keychain），不落明文
- provider 配置走 TOML，可热加载
- 审计日志只写本地 SQLite，只记 token 消耗不记对话内容

---

## 3. Research-Agent 层

```
research-agents/
  ├── lit-search/         # 文献检索
  │   ├── arxiv scan      # 新论文监控 + 筛选
  │   ├── zotero query    # 本地 Zotero 库检索
  │   └── lit-synthesize  # 多篇文献交叉综合
  │
  ├── data-analysis/      # 实验数据分析
  │   ├── transport-plot  # 输运测量绘图 + 拟合
  │   ├── xrd-analyze     # XRD 物相鉴定
  │   ├── rheed-check     # RHEED 振荡分析
  │   └── lab-notebook    # 实验记录结构化
  │
  ├── theory-physics/     # 理论物理
  │   ├── topo-order/     # 拓扑序（模型验证, Lean 辅助, 论文→可计算模型）
  │   ├── sc-theory/      # 超导理论（能隙对称性, 二极管模型, 界面哈密顿量）
  │   └── magnetic-sc/    # 磁性超导耦合（相图, GL 自由能拟合）
  │
  ├── training-pipe/      # 训练 pipeline（未来）
  │   ├── data-prep       # 数据清洗 + 特征工程
  │   ├── model-train     # 训练提交/监控
  │   └── inference       # 本地模型推理
  │
  ├── feishu-delivery/    # 飞书报告
  │   ├── daily-digest
  │   └── result-brief
  │
  └── paper-writing/      # 论文辅助
      ├── draft-section
      ├── figure-caption
      └── ref-check
```

每个 agent 以 MCP server 形式通过 ACP 协议接入 omw-agent。数据分析类用 Python（numpy/scipy/matplotlib），文献类可用 TS 或 Python。

典型交互：
```bash
ai "这周超导二极管相关的新 arxiv 论文"
ai "画今天这组 R-T 曲线，标出 Tc onset"
ai "检查这个 anyon model 的 modular data 是否自洽"
```

---

## 4. Windows 构建策略

分 4 个阶段：

| 阶段 | 目标 | 预计耗时 |
|------|------|---------|
| Phase 1 | 验证上游 Warp Windows 构建（无改动） | 30-60 min |
| Phase 2 | 应用 omw_local 去云端改造（5 文件 cfg 门控） | <1 小时 |
| Phase 3 | 编译 release + 二进制审计（strings grep） | 40-60 min |
| Phase 4 | 打包便携版 / 安装器 | 数小时 |

去云端改动与 Mac 版共用同一套 omw_local feature：
- `warp_core channel config` → 127.0.0.1:0
- `auth credentials` → 空 Firebase URL
- `warp_completer Cargo.toml` → 去 embed-signatures
- `install_remote_server.sh` → 注释中的 URL
- `app Cargo.toml` → omw_local feature

未签名构建，首次启动需手动解除 SmartScreen/Defender 拦截。

---

## 5. 与 oh-my-warp 的差距

| 维度 | oh-my-warp 现状 | 本设计 |
|------|----------------|--------|
| 平台 | Mac only (aarch64) | Windows (x64, 后续 arm64) |
| 打包 | .dmg only | .zip 便携版 + 可选安装器 |
| AI 后端 | 未实现 (v0.3 计划) | Claude + DeepSeek + 本地 endpoint |
| agent 专业化 | 通用 agent | research-agents 5 模块 |
| 品牌 | 过渡图标，二进制仍叫 warp-oss | 改名 omw |

关键风险：
- oh-my-warp v0.3 尚未完成，omw-server/omw-agent 仍为脚手架
- Windows 打包无现成脚本，需自建
- 上游 Warp 的 Windows 构建依赖 conpty/DXC DLL，需验证在本地环境可用

---

## 6. 非目标

- 不做 macOS/Linux 版本
- 不在终端 GUI 中添加科研专属面板（可视化走独立浏览器窗口或终端内绘图）
- 不替换现有 IDE/笔记本环境，仅作为终端增强
- 不处理多用户协作场景
