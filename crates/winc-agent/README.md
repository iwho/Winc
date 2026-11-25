# Winc 原生 Codex 侧边栏

点击 Winc 底部状态栏的 Codex 图标打开或收起侧边栏，也可以从命令面板运行「显示或隐藏 Codex Agent」。空窗口可直接发消息，Winc 会在用户配置目录下用独立的 `codex-chat` 目录启动 Codex App Server。打开本地项目时，Codex 可以连接该项目；编辑器文件或选区只有在主动附加时才加入消息。Codex 执行命令和文件操作，Winc 负责会话界面、权限交互和编辑器导航。

## macOS 构建与使用

在仓库根目录执行以下命令。使用从终端启动的独立 Winc 实例可保留当前 shell 的环境。

    codex --version
    codex login status
    cargo build --release --locked -p winc -p winc-pty
    target/release/winc --new-instance /path/to/local/project

如果 Codex 尚未登录，在终端运行 codex login，再点击面板中的「重新连接」。Codex 使用自己的本机认证配置。Winc 默认从 PATH 查找 codex。macOS GUI 启动的实例还会检查 /opt/homebrew/bin 和 /usr/local/bin。可在用户级 Winc config.json 中指定绝对路径或关闭模块。

    {
      "agent": {
        "enabled": true,
        "executable": "/opt/homebrew/bin/codex"
      }
    }

命令面板中的「Codex：附加当前文件」与「Codex：附加选区」将编辑器中的文本放入待发送上下文。包含未保存文字的上下文会标明来源。正文与上下文合计上限为 256 KiB。按 Enter 发送，Shift+Enter 换行。Option/Alt+↑↓ 取回输入历史。输入 `@` 加文件名后按 Tab，从 Winc 项目索引选择路径。以 `/` 开头时，可选择当前可用的面板命令。同一会话有活动任务时，「指导」向当前轮提交补充输入，「停止」中断当前轮。

「历史」列出当前项目的会话并支持翻页。恢复后显示最近 16 轮，滚动到会话顶部可继续加载。模型与推理强度来自 Codex 的发现接口。默认采用服务器标记的默认模型，已有会话在所用模型仍可用时可沿用该模型。同一 Winc 进程中，同一根目录的多个会话正在运行时会显示写入冲突提醒。

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
| crates/winc-app/src/app/agent/composer.rs | 项目文件索引与可用面板命令的输入补全 |
| crates/winc-app/src/view/agent.rs | 自研渲染器中的侧边栏，复用 Markdown 解析与多行输入 |
| crates/winc-app/src/app/snapshot/agent.rs | 离屏交互验收 |
| crates/winc-config/src/config/fields.rs、config.rs | 用户配置中的 agent.enabled 与 agent.executable |
| crates/winc-i18n/locales/{en-US,zh-CN}/agent.ftl | 中英文侧边栏文案 |

Agent 的面板状态和布局数据在打开时才分配。App Server 使用独立的 stdin、stdout、stderr 管道。后台线程对 stdout 上的逐行 JSON 帧限长，对事件队列施加背压，合并约 12 ms 内连续到达的同一条消息增量。stderr 独立排空，界面只报告经过归类的错误和 JSON-RPC 错误码。Winc 用现有的窗口事件代理收取批次，只更新发生变化的消息和工具卡片，绘制可见的聊天行。工具输出上限 24 KiB，单会话可见文本和输出有 8 MiB 预算，每个面板有 32 MiB 文本预算。预算超出时会淘汰较早的 UI 记录。

## 协议与编辑器行为

已使用 Codex CLI 0.164.0 的本机 Schema 验证字段与方法。以下命令可生成对应安装版本的 Schema。

    codex app-server generate-json-schema --out /tmp/winc-codex-schema

握手用 initialize 和 initialized，随后通过 account/read 检查登录。会话用 thread/start、thread/list、thread/read、thread/resume 和 thread/turns/list。任务用 turn/start、turn/steer、turn/interrupt 和关联的 item、turn、thread 通知。额外字段与未知通知可安全忽略，未支持的服务端请求收到 JSON-RPC 方法错误。服务端请求 ID 支持整数和字符串。 [官方 App Server 文档](https://developers.openai.com/codex/app-server/) · [上游 App Server 实现](https://github.com/openai/codex/tree/main/codex-rs/app-server)

每个会话有明确的本地项目根目录。恢复历史前先通过 thread/read 验证原会话归属。默认权限为 workspace-write、按需审批、用户审查。需要用户决定的命令、补丁和额外权限会显示审批卡片。卡片按服务端声明或对应 v2 请求类型展示此次允许、会话授权、拒绝或取消任务。补丁晚于审批请求到达时，待审卡片会更新。请求被服务端撤销、当前轮结束、连接断开或本地等待超过五分钟后失效。无法完整呈现的审批不会获得授权。

Codex 使用 fileChange 项报告的已完成文件修改会在侧边栏标记为「Codex 报告的修改」。现有文件监听在任务运行期间发现的其他路径标记为「工作区变更」。任务开始时不会把已有 Git 改动导入本轮列表。已完成的补丁可显示 Codex 报告的新增和删除行数。「差异」打开原有 Git Diff，展示该文件的工作区差异，其中可能包含用户先前的改动。Winc 的冲突机制会保留编辑器中的未保存内容。

## 验收命令

    cargo test --locked -p winc-agent -p winc-app -p winc-config -p winc-i18n
    cargo test --locked -p winc-agent --test app_server live_codex_turn -- --ignored
    cargo xtask bench

自动化测试覆盖逐行 JSON、UTF-8 流式合并、命令审批拒绝、晚到补丁的文件审批、任务中断、进程退出与恢复、会话草稿隔离、模型发现、文件跳转、现有 Git Diff 与未保存内容保护、面板调整、聊天滚动及 Winc 中文输入法事件。真实 Codex 测试用本机已登录账号，从 model/list 选择实际可用模型，在临时项目生成并核对文件，再查询和恢复会话。

本机基线与集成构建使用 Rust 1.97.1。仓库固定的 1.99.1 未安装，因此本机复现上述命令时可用 cargo +stable。Windows 和 Linux 的实机界面验证仍待完成。

## 首次集成的历史 Release 数据

在同一台 macOS 14.5 arm64 机器上比较基线 f14933a6 与集成版。两份 release 可执行文件放在同一个目录，交替先后顺序。应用每次启动新进程，保留系统文件缓存。首帧用 WINC_BENCH=startup 的 first_present_epoch_ms 减去启动前的时钟读数，取每种文件、每个版本各 30 次的中位数。RSS 用 WINC_BENCH=memory，启动 2.5 秒后以 ps 采集，取各 5 次的中位数。大文件样本为 200,000 行 Rust 文本，共 11.6 MB。

| 指标 | 基线 | 集成版，面板关闭 | 变化 |
| --- | ---: | ---: | ---: |
| 小文件启动至首帧 | 291.177 ms | 287.538 ms | −1.25% |
| 大文件启动至首帧 | 297.994 ms | 274.646 ms | −7.84% |
| 小文件空闲 RSS | 70.312 MiB | 73.656 MiB | +3.344 MiB |
| 大文件打开后的 RSS | 88.391 MiB | 88.531 MiB | +0.140 MiB |

复测满足首帧退化不超过 5% 与额外空闲 RSS 不超过 5 MB 的目标。采样时未发现 Codex 子进程。

第一轮在不同路径运行两个二进制，20 次大文件首帧的中位数为 312.303 ms 和 346.865 ms，超出目标。随后移除了通用窗口布局中的 Agent 几何数据，采用同目录交替复测。复测的小文件从 main 入口至首帧为 120.824 ms 和 132.118 ms，大文件为 140.148 ms 和 126.990 ms。单次启动数据波动较大，表中结果反映本机测量条件。

## 侧边栏升级报告

当前 Git 分支从 `ac4ede34301e16bbf711e25519fd39ffcd33c74e` 开始，原工作树干净。已有实现使用按需启动的本地 App Server stdio、约 12 ms 事件合并、窗口内会话状态、原生多行输入、工作区监视和 Git Diff。旧聊天能缓存单条布局，但每次绘制都遍历历史以重算纵坐标，宽度变化会重排已加载卡片，文本最多保留 600 张卡片。旧渲染把 Markdown 展平为文字行，审批只有一次允许和拒绝，恢复只取首个历史页。

### 源码和许可证

| 源码 | 实际检查的路径和行为 | 复用决定 |
| --- | --- | --- |
| OpenAI Codex `a0d5b0a661db3b34cd87bdf1c93736b90fd3694b` | `codex-rs/app-server-protocol/src/protocol/{common.rs,v2/item.rs,v2/turn.rs,v2/permissions.rs,v2/thread.rs,item_builders.rs}` 核对 item 类型、审批、引导、历史页和补丁格式。查看 `codex-rs/tui/src/{custom_terminal.rs,tui.rs,tui/event_stream.rs,markdown_render/streaming.rs,chatwidget/streaming.rs,bottom_pane/chat_composer.rs,slash_command.rs}`，以及 `history_cell/snapshots` 和 `pager_overlay/snapshots` 的探索工具、计划、MCP 和历史分页。 | Codex 为 Apache-2.0，上游包含自身 NOTICE。当前提交通过 JSON-RPC 复用安装版 Codex App Server，并按协议原文件路径追踪。源码移植为 0 行。 |
| Agent Hub `860ffff3d2d3d4ee650d1c51fbad3bc3bfbf93fd` | `src/codex/{app_server.rs,thread_item.rs}`、`src/components/{thread_timeline_panel.rs,composer_panel.rs,pending_approval_panel.rs}` 和 `src/diff_view.rs` 分别提供会话事件、GPUI 消息列表、输入候选、审批面板和统一 Diff 解析。当前时间轴遍历所选会话消息生成元素。 | 检出提交未声明许可证，Cargo 清单也没有 license。仅参考交互和结构，源码移植为 0 行。 |
| Zed `17fe8c6117eff6078628bd703a1c4330ab4151d4` | `crates/agent_ui/src/{conversation_view/thread_view.rs,message_editor.rs,context.rs,thread_metadata_store.rs}`，`crates/gpui/src/elements/list.rs`、`crates/markdown/src/markdown.rs` 和 `crates/buffer_diff`。重点是列表项加项内像素偏移的逻辑锚点，以及会话能力驱动的输入候选。 | 选定 Agent、Markdown 和 Diff crate 为 GPL-3.0-or-later。GPUI 为 Apache-2.0，但带完整 UI 运行时。仅参考行为，源码移植为 0 行。 |

仓库快照可通过提交链接查看。 [OpenAI Codex](https://github.com/openai/codex/tree/a0d5b0a661db3b34cd87bdf1c93736b90fd3694b) · [Agent Hub](https://github.com/jk-gan/agent-hub/tree/860ffff3d2d3d4ee650d1c51fbad3bc3bfbf93fd) · [Zed](https://github.com/zed-industries/zed/tree/17fe8c6117eff6078628bd703a1c4330ab4151d4)

1. **直接依赖**。继续使用已有的 `pulldown-cmark 0.13.5`（MIT）、`serde_json` 和 `winit`，连同 Winc 的 `winc-preview`、`winc-typeset`、`winc-syntax`、`winc-render` 和 `winc-editor`。新版 Cargo 清单和锁文件没有增加生产依赖。
2. **纯 Rust 候选**。协议类型、补丁统计和条目合并可以独立于界面。Codex 的 v2 类型 crate 同时依赖历史、回放、RMCP 和多组内部类型，in-process client 还依赖 `codex-core` 和完整 App Server。保留 Winc 现有的薄协议映射，逐项按实际 schema 扩充。Agent Hub 的独立 Diff 解析器来自未声明许可的仓库。
3. **需解耦的候选**。Codex 的流式 Markdown 记录可变块边界，但输出和换行宽度面向终端文本。Zed 的消息编辑器、滚动列表和 Diff 依赖 GPUI 与编辑器实体。在 GUI 中保留对应的交互，用 Winc 文档块哈希、原生文件索引、像素锚点和 Diff。
4. **Winc 适配**。原生侧边栏负责布局、点击目标、代码块复制区、选择高亮、审批按钮和表格滚动。协议状态和审批分支位于 `winc-agent`，卡片材料和会话锚点位于 `app/agent`，编辑器联动沿用现有路径。

### Ratatui 验证

用 Ratatui 0.30.2（MIT）的独立 Release 原型实现 `Backend`。原型用 `TestBackend` 保存单元格和光标，`flush` 把 16×4 Buffer 转成 Winc 的离屏文本绘制，验证两个连续帧以及中文占双列。第二帧绘制了 438 个非背景像素。小部件原型的初始化和释放独立于系统终端。

完整 Codex TUI 的 `tui.rs` 将 `Terminal` 绑定到 `CustomTerminal<CrosstermBackend<Stdout>>`，同时在启动、切换屏幕和暂停时操作 TTY、原始模式和键盘事件流。键盘与鼠标需额外转换。中文 IME 组字、按字形选择、终端滚屏与 Winc 的像素布局也需单独接入。原型验证了绘制转换，未跑通完整 TUI 的这些路径，亦未测量完整嵌入的性能。因此采用 Winc 原生渲染。

### 升级结果

| 区域 | 实际实现 |
| --- | --- |
| 消息 | Winc 文档排版支持标题、列表、引用、GFM 表格、行内样式、代码高亮和代码复制。可选择并复制单张卡片内的渲染文字，也能复制消息原文。宽表格可横向翻动。流式批次只让变化卡片失效。对话中的 Markdown 保留未闭合围栏的可渲染内容，复用哈希未变的布局块。 |
| 滚动 | 每个会话记录卡片序号和卡片内像素偏移。新卡片、懒排版和面板缩放保留阅读锚点。跟随底部、未读提示、回到底部、PageUp/PageDown 和触控板像素滚动已接入。窗口只为可见卡片保留详细布局，其他卡片保留轻量高度估计。上限为每会话 10,000 张卡片或 8 MiB 正文与输出，每面板 32 MiB 正文与输出。 |
| 工具 | 按会话、任务、item ID 合并增量，区分读取、搜索、命令、测试、文件变更、MCP、子 Agent 和计划。连续读取和搜索默认采用相邻单行卡片。内容按需展开，失败和中断状态可见。每项保留至多 24 KiB 输出。 |
| 输入和审批 | 原生输入框、IME、选区、撤销重做、粘贴和会话内草稿继续使用。Enter 发送，Shift+Enter 换行，Option/Alt+↑↓ 浏览历史，`@` 加 Tab 使用文件索引，`/new`、`/resume`、`/diff`、`/model` 映射到现有且可用的面板动作。任务可指导或停止。审批使用原请求 ID 显式响应开启的选项，超时或失效后不可提交。 |
| 编辑器与会话 | 文件及行号跳转、当前文件和选区上下文、已报告变更及行数统计、既有 Git Diff 和未保存缓冲区冲突保护。会话分页恢复、标题通知、独立草稿与阅读位置，以及服务端模型、推理强度、Token 和上下文容量。会话依附规范化的项目根目录，Git worktree 打开为其自身工作区。 |

### 验证缺口和维护风险

- 大型工具输出采用本地限额，超出限额的部分未做协议端按需回取。工具连续阅读分组目前是紧凑呈现，尚无整组折叠。
- 本地文本选择限制在单张卡片。消息里的远程或嵌入图片加载、完整 MCP 输入问答、审批策略规则永久变更、跨进程草稿恢复和自动创建 worktree 隔离未接入。
- Codex 补丁统计来自本轮协议项。现有工作区 Git Diff 仍包含该文件的全部未提交改动。文件监视事件仅证明本轮期间观察到变化，不能归因给 Codex。
- 同一进程的多会话有写入提示。不同 Winc 实例间的写入协调沿用既有编辑器机制。长会话滚动条在未知高度的历史段使用估计。
- App Server v2 的部分字段为实验性。升级本机 Codex 时应重新生成 schema、跑 stdio 回放，并用真实会话检查分页、审批和指导。测试命令识别基于少量已知执行程序，常规无结构命令会显示通用命令。
- macOS 原生 GUI 自动化启动独立测试窗口返回超时。离屏图像和 IME 事件覆盖已执行，系统输入法候选窗、真实触控板惯性及 Windows/Linux 的实机界面仍需验收。

### 手工验收

1. 运行 `target/release/winc --new-instance /path/to/local/project`，打开 Codex，发一条请求让 Codex 返回双语标题、长表格和 Rust 代码块。用四种宽度检查代码复制、表格翻页、字形选择和行号链接。
2. 使用系统中文输入法输入多行内容，验证候选确认、Shift+Enter 换行、Enter 发送、撤销/重做和输入历史。
3. 让 Codex 流式输出较长回复，将视口向上翻动，调整宽度并展开工具卡片。检查未读按钮、回到底部、PageUp/PageDown、指导和中断。
4. 在临时 Git worktree 中预先创建未提交改动和未保存编辑，执行会触发审批的受控任务。检查用户决定、会话范围、Agent 报告行数、原生 Git Diff 与缓冲区冲突提示。

### 本次 Release 性能和回归

基线为升级前的 `ac4ede34`。macOS 14.5 arm64，Rust 1.97.1，工作区 Release 配置。启动为独立进程的小文件首帧，保留系统文件缓存。两个二进制位于同一目录，交替测量，间隔 150 ms。每版取 25 次首帧和 5 次闲置 RSS 的中位数。空闲 RSS 在启动 2.5 秒时采集，采样期间未见 Codex 子进程。

| 原生窗口，面板关闭 | 基线 | 新版 |
| --- | ---: | ---: |
| 进程启动至首帧 | 307.18 ms | 305.13 ms |
| main 入口至首帧 | 117.62 ms | 121.22 ms |
| 空闲 RSS | 75.70 MiB | 76.33 MiB |

同目录逐对相减的首帧中位差为 +2.55 ms。前两轮各 15 次样本给出 169.10→165.94 ms 和 175.99→230.36 ms，说明启动绝对值有明显波动。本次结果仅用于本机对照。

离屏测试采用相同事件输入、窗口和渲染器。每版每档用三个独立进程交替执行。滚动和缩放各 60 步，每步绘制两帧。流式测试连续追加 60 个 Markdown 批次，CPU 为进程累计增量。面板 RSS 在初次绘制后采集，包含测试宿主和离屏画布。该数据未纳入 App Server 子进程、显示服务或真实键鼠输入。实际保留数包含后续添加的流式卡片。

| 注入历史 / 指标 | 基线 | 新版 |
| --- | ---: | ---: |
| 100 / 实际保留卡片 | 101 | 101 |
| 100 / 初次面板 RSS | 32.36 MiB | 32.52 MiB |
| 100 / 首次布局、滚动、缩放 | 7.74 / 2.40 / 2.51 ms | 7.84 / 2.60 / 2.81 ms |
| 100 / 流式 CPU | 230 ms | 260 ms |
| 1,000 / 实际保留卡片 | 600 | 1,001 |
| 1,000 / 初次面板 RSS | 33.66 MiB | 34.03 MiB |
| 1,000 / 首次布局、滚动、缩放 | 18.16 / 2.85 / 3.60 ms | 14.40 / 3.57 / 3.70 ms |
| 1,000 / 流式 CPU | 320 ms | 290 ms |
| 10,000 / 实际保留卡片 | 600 | 10,000 |
| 10,000 / 初次面板 RSS | 33.84 MiB | 39.56 MiB |
| 10,000 / 首次布局、滚动、缩放 | 61.07 / 2.55 / 3.42 ms | 10.76 / 2.71 / 2.87 ms |
| 10,000 / 流式 CPU | 250 ms | 270 ms |

短会话的富文本绘制增加了离屏滚动、缩放和流式 CPU 成本。注入 10,000 条时，新版实际保留更多历史，初次排版更快，RSS 增加约 5.72 MiB。上述基准分别衡量各版本自身的保留策略，600 张之后无法比较相同的常驻历史数量。

- `cargo +stable build --offline --locked --release -p winc` 通过。
- `cargo +stable test --offline -p winc-app -p winc-agent -p winc-typeset -p winc-config -p winc-i18n` 在系统权限下通过。包括 1,542 个应用测试和 121 个 typeset、配置及本地化测试，以及协议帧和 stdio 集成测试。额外运行了审批过期、失败状态和行数统计测试。
- 本机 Codex CLI 0.164.0 的真实回合通过，涵盖模型发现、文件修改、流式完成、历史查找和恢复。stdio 回放另覆盖未知事件、交错的工具增量、审批、指导、停止和断线。
- 离屏使用 280、400、600、700 px 四档重新检查截图，修正 280 px 上代码复制按钮遮挡首行的问题。Winc IME 回放通过预编辑和中文提交。

复现 Release 离屏基准和截图可运行以下命令。

    cargo +stable test --locked --release -p winc-app agent_transcript_release_benchmark -- --ignored --nocapture
    WINC_SNAPSHOT_DIR=/tmp/winc-sidebar-shots cargo +stable test --locked -p winc-app agent_rich_snapshot_and_selection -- --nocapture

代码改动约 +2.4k / −0.5k 行，包含回放、快照断言、本地化和 Release 基准。Cargo 生产依赖增加 0 项，三个参考仓库的代码移植 0 行。
