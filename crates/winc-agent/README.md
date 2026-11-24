# Winc 原生 Codex 侧边栏

在 Winc 打开本地项目文件夹，在命令面板运行「显示或隐藏 Codex Agent」。面板展开后可点击标题栏的 Codex 按钮收起。第一次打开侧边栏时，Winc 启动项目专属的 Codex App Server。Codex 执行命令和文件操作，Winc 负责会话界面、权限交互和编辑器导航。

## macOS 构建与使用

在仓库根目录执行以下命令。使用从终端启动的独立 Winc 实例可保留当前 shell 的环境。

    codex --version
    codex login status
    cargo build --release --locked
    target/release/winc --new-instance /path/to/local/project

如果 Codex 尚未登录，在终端运行 codex login，再点击面板中的「重新连接」。Codex 使用自己的本机认证配置。Winc 默认从 PATH 查找 codex。macOS GUI 启动的实例还会检查 /opt/homebrew/bin 和 /usr/local/bin。可在用户级 Winc config.json 中指定绝对路径或关闭模块。

    {
      "agent": {
        "enabled": true,
        "executable": "/opt/homebrew/bin/codex"
      }
    }

命令面板中的「Codex：附加当前文件」与「Codex：附加选区」将编辑器中的文本放入待发送上下文。包含未保存文字的上下文会标明来源。正文与上下文合计上限为 256 KiB。按 ⌘Enter 发送，Enter 换行。其他平台按 Ctrl+Enter 发送。同一会话有活动任务时，「发送」禁用，可用「停止」提交中断；切换到别的会话可独立运行任务。

「历史」列出当前项目的会话并支持翻页。恢复后显示最近 16 轮。模型与推理强度来自 Codex 的发现接口。默认采用服务器标记的默认模型，已有会话在所用模型仍可用时可沿用该模型。同一 Winc 进程中，同一根目录的多个会话正在运行时会显示写入冲突提醒。

空闲时隐藏面板会释放连接。已有任务或待审批请求时，隐藏会保留连接；重新打开可停止任务或处理审批。隐藏期间收到新的审批，Winc 会提示。关闭窗口或退出 Winc 会结束关联进程。通信中断后可重新连接并恢复会话，提交结果未知的请求保留草稿，重连不会重新发送。

## 架构与模块

| 实现 | 职责 |
| --- | --- |
| crates/winc-agent/Cargo.toml | 独立 Rust crate 的依赖与平台特性 |
| crates/winc-agent/src/lib.rs | 与 UI 无关的 AgentProvider、命令、会话、审批与事件类型 |
| crates/winc-agent/src/codex.rs | App Server 启动、握手、收发、ID 与超时、进程监督 |
| crates/winc-agent/src/codex/shutdown.rs | Unix 进程组和 Windows Job Object 清理 |
| crates/winc-agent/src/protocol.rs | Codex v2 方法和事件映射、工作区检查、流式合并、审批响应 |
| crates/winc-agent/tests/app_server.rs、mock-app-server.sh | stdio 集成测试和需本机登录的真实 Codex 测试 |
| crates/winc-app/src/app/agent.rs | 窗口级生命周期、会话状态、草稿、事件处理 |
| crates/winc-app/src/app/agent/editor.rs | 编辑器上下文、路径与行号解析、文件导航和 Diff |
| crates/winc-app/src/app/agent/sessions.rs | 历史翻页、会话切换、模型与推理强度 |
| crates/winc-app/src/view/agent.rs | 自研渲染器中的侧边栏，复用 Markdown 解析与多行输入 |
| crates/winc-app/src/app/snapshot/agent.rs | 离屏交互验收 |
| crates/winc-config/src/config/fields.rs、config.rs | 用户配置中的 agent.enabled 与 agent.executable |
| crates/winc-i18n/locales/{en-US,zh-CN}/agent.ftl | 中英文侧边栏文案 |

Agent 的面板状态和布局数据在打开时才分配。App Server 使用独立的 stdin、stdout、stderr 管道。后台线程对 stdout 上的逐行 JSON 帧限长，对事件队列施加背压，合并约 12 ms 内连续到达的同一条消息增量。stderr 独立排空，界面只报告经过归类的错误和 JSON-RPC 错误码。Winc 用现有的窗口事件代理收取批次，只更新发生变化的消息和工具卡片，绘制可见的聊天行。工具输出上限 24 KiB，单会话可见文本和输出有 8 MiB 预算，每个面板有 32 MiB 文本预算。预算超出时会淘汰较早的 UI 记录。

## 协议与编辑器行为

已使用 Codex CLI 0.164.0 的本机 Schema 验证字段与方法。以下命令可生成对应安装版本的 Schema。

    codex app-server generate-json-schema --out /tmp/winc-codex-schema

握手用 initialize 和 initialized，随后通过 account/read 检查登录。会话用 thread/start、thread/list、thread/read、thread/resume 和 thread/turns/list。任务用 turn/start、turn/interrupt 和关联的 item、turn、thread 通知。额外字段与未知通知可安全忽略，未支持的服务端请求收到 JSON-RPC 方法错误。服务端请求 ID 支持整数和字符串。 [官方 App Server 文档](https://developers.openai.com/codex/app-server/) · [上游 App Server 实现](https://github.com/openai/codex/tree/main/codex-rs/app-server)

每个会话有明确的本地项目根目录。恢复历史前先通过 thread/read 验证原会话归属。默认权限为 workspace-write、按需审批、用户审查。需要用户决定的命令、补丁和额外权限会显示审批卡片。卡片支持拒绝或仅此次允许，额外权限限定于当前轮。补丁晚于审批请求到达时，待审卡片会更新。无法完整呈现请求的审批保留拒绝操作。

Codex 使用 fileChange 项报告的已完成文件修改会在侧边栏标记为「Codex 报告的修改」。现有文件监听在任务运行期间发现的其他路径标记为「工作区变更」。任务开始时不会把已有 Git 改动导入本轮列表。「差异」打开原有 Git Diff，按该文件显示整个现有工作区差异，其中可能同时包含用户先前的改动。Winc 的冲突机制会保留编辑器中的未保存内容。

## 验收命令

    cargo test --locked -p winc-agent -p winc-app -p winc-config -p winc-i18n
    cargo test --locked -p winc-agent --test app_server live_codex_turn -- --ignored
    cargo xtask bench

自动化测试覆盖逐行 JSON、UTF-8 流式合并、命令审批拒绝、晚到补丁的文件审批、任务中断、进程退出与恢复、会话草稿隔离、模型发现、文件跳转、现有 Git Diff 与未保存内容保护、面板调整、聊天滚动及 Winc 中文输入法事件。真实 Codex 测试用本机已登录账号，从 model/list 选择实际可用模型，在临时项目生成并核对文件，再查询和恢复会话。

本机基线与集成构建使用 Rust 1.97.1。仓库固定的 1.99.1 未安装，因此本机复现上述命令时可用 cargo +stable。Windows 和 Linux 的实机界面验证仍待完成。

## Release 性能对照

在同一台 macOS 14.5 arm64 机器上比较基线 f14933a6 与集成版。两份 release 可执行文件放在同一个目录，交替先后顺序。应用每次启动新进程，保留系统文件缓存。首帧用 WINC_BENCH=startup 的 first_present_epoch_ms 减去启动前的时钟读数，取每种文件、每个版本各 30 次的中位数。RSS 用 WINC_BENCH=memory，启动 2.5 秒后以 ps 采集，取各 5 次的中位数。大文件样本为 200,000 行 Rust 文本，共 11.6 MB。

| 指标 | 基线 | 集成版，面板关闭 | 变化 |
| --- | ---: | ---: | ---: |
| 小文件启动至首帧 | 291.177 ms | 287.538 ms | −1.25% |
| 大文件启动至首帧 | 297.994 ms | 274.646 ms | −7.84% |
| 小文件空闲 RSS | 70.312 MiB | 73.656 MiB | +3.344 MiB |
| 大文件打开后的 RSS | 88.391 MiB | 88.531 MiB | +0.140 MiB |

复测满足首帧退化不超过 5% 与额外空闲 RSS 不超过 5 MB 的目标。采样时未发现 Codex 子进程。

第一轮在不同路径运行两个二进制，20 次大文件首帧的中位数为 312.303 ms 和 346.865 ms，超出目标。随后移除了通用窗口布局中的 Agent 几何数据，采用同目录交替复测。复测的小文件从 main 入口至首帧为 120.824 ms 和 132.118 ms，大文件为 140.148 ms 和 126.990 ms。单次启动数据波动较大，表中结果反映本机测量条件。

## 已知限制

- 当前支持本地项目。SSH 和 WSL 项目会得到明确提示。
- 聊天渲染支持段落、标题、列表、表格、围栏代码和基本本地链接。原文复制保留 Markdown。复杂行内样式和嵌入内容采用简化显示。
- 暂不提供活动任务中发送后续指导、外部身份登录、MCP 问答、逐段回滚或自动 Worktree 隔离。
- 会话历史恢复只加载最近 16 轮。更早的对话分页、跨 Winc 进程的并发写入提示和系统输入法实机覆盖为后续工作。
