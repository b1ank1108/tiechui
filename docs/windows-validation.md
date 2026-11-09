# Windows 验证记录

## 范围与结论

- 分支：`fix/windows`。Phase A 基线为 `3e8d7d4`；本次 A2 / B 代码验证至
  `073b6ab`，之后的文档提交不改变产品行为。
- 环境：真实 Windows 11、PowerShell、MSVC、Rust 1.95.0、nextest 0.9.151、
  Python 3.14.4、Node 24.18.0、Git for Windows。
- A2 完成后：820 项测试通过；B 阶段修复并增加回归测试后：**824 项通过，
  0 失败、0 跳过**。fmt、完整 clippy、doctests 均通过；当前没有可运行的 doctest。
- 每个提交前均执行 `cargo fmt --all --check` 和
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`。
- 每个独立场景使用新的临时 `TIECHUI_HOME`、临时工作目录和本地流式
  OpenAI Chat Completions mock。恢复、授权持久化和并发读取场景只复用各自的
  临时 home；未使用真实 API key 或真实 `~/.tiechui`。
- B1 验证发布版 v0.2.0 的安装器；B2–B6 使用本分支构建的 CLI。
  不将分支验证结果等同于已发布二进制的完整功能验证。
- 下列 open 项尚未解决。测试全绿不表示这些设计问题、偶发问题和覆盖缺口已关闭。

详细原始命令、JSON-RPC、SSE 请求及前后对照保存在本地忽略目录
`.local/` 中，未随文档提交。下文同时记录关键输出、源码位置和验证方式，
以便在没有这些本机日志的情况下继续调查。

## 待处理问题

### B-05：跨会话删除运行中的插件会部分删除包

- **严重度 / 状态：major / open。**
- 复现：安装最小 SDK 插件，在一个会话中保持其扩展进程和 MCP server 运行，
  另一个进程执行 `chui plugin remove winfixture`。
- 实际结果：退出码 2，仅返回通用的 `Access denied (os error 5)`。
  原有 5 个包文件中只剩 `node.exe` 和 `sdk.mjs`；manifest、入口及 MCP 脚本
  已被删除。关闭持有进程的会话后，再次 remove 成功。
- 根因：Windows 锁定正在执行的映像，但递归删除遇到该锁之前已经删除其他文件。
  这与 A2.9 的“同会话停止尚未完成”竞态不同。
- 待定方案：跨会话包租约并显式停止 / 重试；或评估 Windows rename 约束后，
  使用 tombstone 与延迟清理；或设计具备 TOCTOU 处理的删除前检查。
  错误至少应包含包路径及关闭其他会话的提示。
- 证据：本地 `a2b/b4-plugin-summary.json`，包含运行进程、完整错误及前后文件列表。
  尚未用猜测性的递归删除重试掩盖该问题。

### B-07：ConPTY 启动依赖客户端回答，且 attach 重放启动查询

- **严重度 / 状态：major / open；本轮按要求未实现产品修复。**
- 创建终端后，`terminal/attach` 只返回 `ESC[6n`、`seq=4`、`exit=null`。
  发送命令并等待 2 秒，缓冲区和序号保持不变。
- 写入光标报告 `ESC[1;1R` 后，命令立即继续并输出 `B5-42`。
  后续 attach 仍包含已经回答过的 `ESC[6n`；模拟重新连接客户端再次回答时，
  第二份报告进入已运行 shell 的输入。
- 根因：portable-pty 0.9 使用 `PSEUDOCONSOLE_INHERIT_CURSOR`；启动查询依赖
  客户端，而原始输出回放没有区分一次性的终端控制交互。
- 待定方案：Windows 服务端回答启动查询并将其排除在回放之外；或分离实时控制
  流量与回放，跟踪并过滤已回答的启动查询。选择前需确定客户端光标原点语义。
- 证据：本地 `a2b/b5-summary.json`、`a2b/b5-app-server-rpc.json`，保留原始
  转义序列、请求顺序和等待前后输出。

### A-05：一次配置保存返回 access denied

- **严重度 / 状态：major / open，偶发。**
- 添加远程 MCP 配置时，临时 `config.toml` 的一次保存返回 OS error 5。
- 根因未知；后续 app-server、30 次压力迭代及完整测试均通过。
  没有证据将其归因于扫描器或某个特定并发操作。
- 待定方案：复现时抓取准确的失败文件操作及句柄；或增加可选的配置保存压力测试
  和句柄生命周期诊断。不添加无根因依据的通用重试。
- 证据：本地 `app-server-env-fix.log`，以及后续 `app-server-stress.log`。

### A-19：一次终端退出码异常

- **严重度 / 状态：major / open，偶发。**
- 原生并行 `cargo test` 中，预期 `exit 3` 的终端一次返回
  `-1073741510`（`0xC000013A`）。后续 30 次原生 workspace 重复运行和
  30 次 nextest 终端压力迭代均通过。
- 根因未知；未修改预期退出码，也未将控制台终止状态替换为 3。
- 待定方案：隔离控制台控制事件，记录 ConPTY 关闭、退出观察和控制事件时序；
  或保留问题，等待稳定复现后再修改生命周期行为。
- 证据：本地 `cargo-test-final.log`、`workspace-bin-stress.log`、
  `app-server-stress.log`。

### A-20：一次原生并行 exec 测试整体变慢

- **严重度 / 状态：major / open，偶发。**
- 同一次运行中 5 个 module-exec 用例失败：首份输出或短命令完成超过原有等待界限。
  后续带临时诊断的 50 次原生并行运行未复现。
- 根因尚未确定，可能的排查点是进程创建、输出读取、等待路径或机器调度，
  这些是调查方向而非已确认原因。
- 待定方案：分段记录 spawn / output / wait 时序并抓取 Windows 进程调度信息；
  或设置独立压力任务。未放宽现有超时，临时诊断已移除。
- 证据：本地 `cargo-test-repeat-2.log`、`exec-bin-stress.log`。

### B-01：发布安装器误报缺少 Git Bash

- **严重度 / 状态：minor / open。**
- v0.2.0 安装器提示 bash 工具需要 Git for Windows；同机 `chui doctor`
  能定位 `C:\Program Files\Git\bin\bash.exe` 并成功。
- 根因：安装器检查 `Get-Command bash`，产品 `resolve_shell()` 还检查 Git 安装布局。
- 待定方案：统一 Git 布局探测，或把文案明确为 bash 不在 PATH，而不是暗示 Git 未安装。
  测试时没有改写下载到的发布安装器。
- 证据：本地 `a2b/b1-transcript.txt`、`a2b/release-install.ps1`。

### B-06：本地 marketplace 显示 verbatim 路径

- **严重度 / 状态：minor / open，显示设计问题。**
- add / list 成功，但 list 直接显示规范化保存的 `\\?\E:\...`。
- 根因：存储保留规范路径身份，显示直接使用该字符串；不是 clone 失败。
- 待定方案：仅在显示时去除 verbatim 前缀，保留存储身份；或保留原样以强调原生路径。
- 证据：本地 `a2b/b4-marketplace-summary.json` 的实际输出和存储记录。

### B-08：当前 schema 没有 git/diff RPC

- **严重度 / 状态：minor / open，接口范围问题，不是 Windows 专属失败。**
- `git/diff` 返回 `-32601`、`unknown method git/diff`；
  `schema/app-server.ts` 中也没有对应接口。
- `git/show` 与 `fs/read` 可取得差异两侧，原生 `git diff` 输出符合预期。
- 待定方案：明确由客户端计算 diff，或另行设计 diff RPC；本轮没有扩展 schema。
- 证据：本地 `a2b/b5-summary.json` 的 RPC 错误、两侧内容及原生 diff 输出。

### B-09：尚未在真正没有 Git 的 Windows 上验证

- **严重度 / 状态：minor / open，覆盖缺口。**
- 尝试清理子进程 PATH / ProgramFiles 后，仍找到本机标准 Git 安装。
  原生探针观察到 ProgramFiles 为系统值；其恢复原因尚未确认。
- 已验证：无效 `TIECHUI_GIT_BASH_PATH` 返回清楚的 Git Bash 安装 / override 提示；
  有效 override 成功；resolver 单测覆盖无 PATH、无 ProgramFiles、无 override。
- 待定方案：在未安装 Git 的干净 VM 上补测，或提供进程级文件系统 / 环境隔离。
  没有移动或卸载用户现有 Git 安装。
- 证据：本地 `a2b/native-probe.json`、`a2b/b2-git-bash-truly-missing.json`。

## 已解决：Phase A

| 编号 | 原症状与根因 | 严重度 | 修复 / 状态 |
| --- | --- | --- | --- |
| A-01 | Rust、nextest、MSVC linker 等本机依赖缺失 | blocker | 本地安装工具链，无仓库提交 |
| A-02 | 测试 `env_clear()` 丢失必要 Windows 环境，Git / ConPTY 启动失败 | major | `69c9f28`、`e106c0a`；窄测试白名单 |
| A-03 | app-server fixture 假设 POSIX 分隔符、换行和 shell 语法 | minor | `5881b76` |
| A-04 | Windows 整文件强制锁阻塞会话列举及读取 | major | `76fe368`；数据范围之外的单字节 writer 锁 |
| A-06 | rewind 对目录 `sync_all()` 返回 OS error 5，阻止编辑 | blocker | `cd21a46`；目录 fsync 仅用于 Unix，保留文件同步和原子发布 |
| A-07 | OAuth 无条件读取 `/dev/urandom` | major | `618d419`；Windows BCrypt 系统随机源 |
| A-08 | ConPTY 测试不回答启动光标查询 | minor | `3532b45`；A2.6 后续完善测试逻辑，产品问题见 B-07 |
| A-09 | append-only Windows 会话句柄没有截断残缺尾行的权限 | major | `c069486`；writer 锁内使用可写句柄并 seek EOF |
| A-10 | 等 ConPTY 输出 EOF 后才检查退出，客户端收不到退出通知 | major | `dc3f6f5`；Windows 退出观察器关闭 master，允许排空输出 |
| A-11 | 其他 CLI / provider fixtures 同样清空必要 Windows 环境 | major | `e106c0a`；共享测试环境辅助函数 |
| A-12 | rewind 拒绝绝对盘符路径，批准的外部编辑失败 | major | `802bf8c`；允许绝对前缀，继续拒绝 drive-relative / traversal |
| A-13 | Git 将本地 verbatim clone 参数当作远端地址解析 | major | `5654acd`；仅转换 Windows 本地 clone 参数，保留存储身份 |
| A-14 | 路径断言 / golden 使用 POSIX 字面值 | minor | `37fa7ba`；别名构造与换行策略由 A2.2 / A2.4 进一步修正 |
| A-15 | replay header 的 `/` 不是 Windows 绝对 cwd | minor | `10cea1a`；仅重绑定临时 fixture header |
| A-16 | shell fixtures 假设 `/bin/sh`、POSIX 路径及退出表示 | minor | `f83f452`；信号状态的产品修复见 A2.1，不再将 3840 作为预期 |
| A-17 | fixture 受 checkout EOL / 用户 Git 配置影响 | minor | `cef2307`；测试仓库保留 autocrlf=false，checkout 补偿由 A2.4 移除 |
| A-18 | 插件 fixture 的 JSON 路径、exe 后缀和停止等待不适配 Windows | minor | `3e8d7d4`；A2.5 / A2.9 继续修复命令和停止状态竞态 |

A-05 / A-19 / A-20 仍为 open，详见前文；未计入“已解决”。

## 已解决：A2 与 B 新发现

| 编号 | 证据与根因 | 修复 |
| --- | --- | --- |
| A2.1（major） | exact `resolve_shell()` 返回 Git `bin\bash.exe`；原生父进程观察 TERM=3840、KILL=2304、INT=512，但 exit 3=3、exit 255=255 | `4823dcf`：仅 Git Bash 路径解码 MSYS 信号状态；bash / hooks 报告 TERM=143，普通 MCP / plugin 子进程状态不变 |
| A2.2（minor） | verbatim PathBuf join 会移除 `..`，原别名测试实际没有别名 | `52fc996`：从未 canonicalize 的 temp.path() 构造别名，恢复无条件 assert_ne |
| A2.3（minor） | Windows master 消失后 resize 返回 terminal has exited，Unix 则成功 | `3635564`：已退出终端 resize 为 no-op Ok，活跃终端错误仍传播 |
| A2.4（minor） | checkout EOL 导致 golden / SSE 测试依赖 CRLF 补偿 | `0173cfd`：`* text=auto eol=lf`，移除仅为 checkout 添加的归一化；autocrlf=true 新克隆 85 项测试通过 |
| A2.5（minor） | 等待循环把 Failed / Disabled 也视作仍需等待；fixture 改写了作者的命令形式 | `97ae424`：只等 Starting / Ready；复制 server.exe，但保持 `./server` manifest，11 项测试通过 |
| A2.6（minor） | ends_with 判断可能漏掉查询，且未记录是否已经回复 | `7ed7dd2`：contains + answer once；14 项 app-server 测试通过 |
| A2.7（minor） | Unix TryLockError 转 io::Error 后丢失原始错误文本 | `2f734da`：保留原始类型及精确文本；Unix 专用断言未在本机执行 |
| A2.8（minor） | timeout /t 10 在 NUL stdin 下立即退出 | `0762aad`：使用 `ping -n 11 127.0.0.1 >nul`，实际覆盖取消活跃命令 |
| A2.9（major） | 全量首次 819/820；MCP 已报 Stopped，异步 kill 尚未完成，删除 exe 仍报 OS error 5 | `83d79d9`：Windows reap 完再报 Stopped；全量 820/820、插件压力测试 20/20 |
| B-02（major） | 单次 grep 搜索一个小文件即可触发 0xC00000FD；find 单独成功；CLI 主栈保留 1 MiB，搜索 future 内嵌 64 KiB + 8 KiB 缓冲区 | `91bf9c7`：两缓冲区改为堆上 Vec，不提高栈大小、不改变搜索输出；真实 CLI 回归先失败后通过 |
| B-03（major） | 裸 npx 为 program not found；显式 npx.cmd / Python server 均 Ready | `00020ef`：Windows 原生 spawn 返回 NotFound 后，才在有效 child PATH 中查裸命令的 .cmd / .bat；保留 exe 优先与 std 参数转义 |
| B-04（major） | taskkill /F 绕过析构，Git Bash 及原生子进程仍 alive；Ctrl+C 原本可清理 | `3866cea`：armed Job 设置 KILL_ON_JOB_CLOSE；显式 disarm 清除该标志，保留原有 detach 语义 |

补充测试提交 `073b6ab` 让新 grep CLI fixture 使用测试 runner 的 PATH，
不假设 rg 安装在 `/usr/bin`；例如 Homebrew 的安装路径可能不同。
Unix 产品行为和错误文本没有为通过 Windows 测试而放宽。

### A2.4 的换行说明

`git add --renormalize .` 没有产生已跟踪内容变化。扫描 index 中的 CR 字节时，
仅二进制 PNG 含 CR，已跟踪文本均为 LF。

新增 attributes 不会追溯改写旧工作树；原有工作树仍可能保留 CRLF。
新克隆显式使用 `core.autocrlf=true`，其文本 checkout 为 LF，skills / providers
的 85 项测试通过。测试内部建立的 Git 仓库仍设置 `core.autocrlf=false`，
用来隔离用户配置；B5 的 autocrlf=true 场景为单独的显式验证。

### B-03 的环境边界与参数说明

- 未增加任何默认继承环境变量。现有 HOME / PATH / SYSTEMROOT / WINDIR /
  TEMP / TMP 足以启动本地离线 npx 和 Python MCP fixtures。
- Python server 未收到 COMSPEC、PATHEXT、APPDATA、LOCALAPPDATA、USERPROFILE。
  cmd / npx 自行合成 COMSPEC、PATHEXT 及 npm 元数据，不代表产品扩大白名单。
- 测试覆盖 PATH 包含空格、`.cmd` / `.bat`、原样参数 `hello & goodbye`、
  native exe 优先以及不泄漏 APPDATA。未拼接 `cmd /C` 命令字符串。
- npx 的全局 prefix 元数据仍出现未展开的 `${APPDATA}` 默认值。
  这里验证的是本地离线 MCP，不声称网络包安装及真实全局 profile 行为已覆盖。

## Phase B 场景结果

| 场景 | 已观察结果与边界 |
| --- | --- |
| B1 安装 | 下载 v0.2.0 PowerShell 安装器；校验资产、两个 CLI version、doctor、重复升级均通过，升级未重复加 PATH。npm 全局安装使用隔离 prefix / cache，shims、version、doctor 正常。安装目录与 cache 已删除，真实用户 PATH 精确恢复；未覆盖既有用户安装 |
| B2 文件 | LF / CRLF read、edit、write 保持预期字节；空格、`测试/文件.txt`、C: / E: 跨盘、380 字符路径成功。grep 修复后，完整场景的 26 份工具结果均返回 |
| B2 shell | 简单命令、exit 7、Windows 路径参数、TERM=143、shell_list / shell_output / shell_stop 通过；缺 Git 的覆盖边界见 B-09 |
| B3 权限 | `.GIT\config`、`.Env`、`.Vscode\settings.json` 仍受保护；规则 / 参数的四种斜线组合通过；allow-listed 放行只读 pwd、拒绝需确认的重定向写入，deny 阻止二者，yolo 在临时目录可写普通文件 |
| B4 MCP | npx、npx.cmd、Python server 均 Ready，且最终 JSON-RPC 会话实际调用 echo 工具成功 |
| B4 插件 | 使用真实 sdk/typescript runtime 编写最小插件，plugin add、能力授权、headless 工具 allow rule 均通过，返回 `SDK_PLUGIN_OK {"message":"Windows"}`。本机运行 JS runtime，不等同于验证文档中的 Node 25 TypeScript 源码执行方式 |
| B4 hook / subagent | command hook 接收 stdin JSON，HOOK_CONTEXT 注入 provider；被 TERM 终止的 hook 未阻塞 turn。Scout 返回 CHILD_OK，父任务返回 PARENT_OK，共 3 次 mock 请求 |
| B4 进程 | 普通退出、真实控制台 Ctrl+C、taskkill /F 后，Bash / 原生子进程及 Python MCP 均退出。最后 Get-Process 和 fixture 命令行审计无残留；插件正常退出后的 PID 也不存在 |
| B5 RPC | initialize、session/open、mock turn、文件读、Git 操作、undo / redo / rewind、settings / provider reload 通过；stdout 每行均为 JSON-RPC |
| B5 Git | 显式 fixture-local autocrlf=true：unborn stage / unstage、已跟踪文件的 status / stage / unstage / discard 通过；discard 恢复 CRLF 且状态干净。diff 接口问题见 B-08 |
| B5 terminal | create / write / resize / exit 3 / kill 通过；自然退出后 resize 返回成功；kill 删除条目后，对已不存在 ID 报错。启动与 replay 问题见 B-07 |
| B6 恢复 / 锁 | resume 使用同一 JSONL，历史 assistant 回复进入下一次请求；一个进程持 writer 时，另一个可列举并读取 6 条有效记录；竞争 writer 清楚地返回锁错误 OS error 33 |
| B6 强杀 | 5 次 turn 中 taskkill /F，实际文件仍为有效完整 JSONL，5 次恢复均退出 0、保持同一 ID。另在其中 3 次强杀后注入残缺尾行，下一次写入正确截断并留下有效 JSONL；不把人工注入描述成自然发生的损坏 |

会话文件位于临时 home 的 `sessions/<encoded canonical cwd>/<id>.jsonl`；
rewind 位于 `state/rewind/<24-hex cwd hash>/<24-hex session hash>/`，包含
manifest 与 blobs，cwd 层另有 writer.lock。

## A2 / B 新提交索引

| Commit | Subject |
| --- | --- |
| `4823dcf` | fix(exec): decode Git Bash signal statuses on Windows |
| `52fc996` | test(config): build real aliases from non-verbatim paths |
| `3635564` | fix(workspace): make resize after terminal exit a no-op |
| `0173cfd` | test: enforce LF checkouts instead of normalizing fixtures |
| `97ae424` | test(plugins): preserve extensionless server commands |
| `7ed7dd2` | test(cli): answer ConPTY cursor queries only once |
| `2f734da` | fix(session): preserve Unix writer lock error text |
| `0762aad` | test(tui): keep clipboard cancellation fixture alive on Windows |
| `83d79d9` | fix(mcp): reap Windows servers before reporting stopped |
| `00020ef` | fix(plugin-api): resolve bare Windows batch commands on PATH |
| `3866cea` | fix(exec): terminate armed Windows jobs when their owner exits |
| `91bf9c7` | fix(editing): keep search stream buffers off the async stack |
| `073b6ab` | test(cli): resolve search fixtures from the runner PATH |

## 验证命令与 CI 提案

Windows 直接调用 cargo；未移植 POSIX-only 的 just / PTY smoke suite。
每次独立运行先设置新的临时 `TIECHUI_HOME`。实际执行的主要检查为：

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features --locked --no-fail-fast
cargo test --workspace --all-features --locked --doc --no-fail-fast
```

本地忽略的 `.config/nextest.toml`：

```toml
[profile.default]
slow-timeout = { period = "30s", terminate-after = 2 }
```

CI **尚未应用修改**；已准备并检查可应用的本地提案：

1. Windows job 使用 nextest 与 `--no-fail-fast`，保留独立 doctests。
2. 每个测试使用上述两段 30 秒超时，job 设置 30 分钟超时。
3. 保留 `.gitattributes` 的 LF checkout 策略，在 autocrlf=true 下检查工作树；
   不重新添加测试侧 CRLF 归一化。
4. 完整验证变绿后移除 `continue-on-error`；保留 fmt / clippy 检查。
5. 保留临时 home 隔离，不改变 Unix jobs，不移植 POSIX PTY suite。

本机验证成功不是 GitHub-hosted Windows CI 已运行成功的声明。
