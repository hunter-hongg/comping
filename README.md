# comping

终端里的编程助手。能读文件、改代码、跑命令，模型可换。

```
┌ 对话 ───────────────────────────────────────┐┌ 状态 ──────────┐
│ 你 │ 重构一下 parser.rs                     ││ 模型            │
│ 我先看看文件内容。                          ││ claude-sonnet   │
│ ▸ ✓ read_file  {"path":"parser.rs"}         ││                │
│  parser.rs (240 行)                         ││ 上下文          │
│ ...                                        ││ ████░░░░  42%  │
│ 改好了，抽出�� Tokenizer 那一段：             ││ 32k / 200k     │
│ ```rust                                    ││                │
│ struct Parser { ... }                      ││ 用量            │
│ ```                                        ││ 输入 28k       │
│ 12.4s · ↓1.2k · 缓存 78%                   ││ 输出 4.1k      │
├──────────────────────────────────────────────┤│                │
│ ❯ 把测试也更新一下_                         ││                │
└──────────────────────────────────────────────┴────────────────┘
```

## 特点

**模型中立。** 中间消息表示不泄漏任何厂商私有格式，换模型只改配置：

```toml
provider = "anthropic"          # anthropic / openai / gemini
model = "claude-sonnet-4-5"
```

DeepSeek、vLLM、Ollama、OpenRouter 都走 OpenAI 兼容协议，加一段配置即可：

```toml
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key = "sk-..."
model = "deepseek-chat"
```

**工具并行执行。** 同一轮里多个互不依赖的调用并发跑，语义化重试（429/5xx 指数退避 + jitter）。

**权限分级。** 只读工具自动执行；写文件和跑命令要确认，确认面板里给的是 diff 和命令全文，不是"确定执行吗"。

**上下文自动压缩。** 超过阈值时先让模型总结早期对话再折叠，保留"它在干什么"的判断，而不是简单截断。

## 快速开始

```sh
export ANTHROPIC_API_KEY=sk-...
cargo run --release
```

或一次性提问（适合脚本和 CI）：

```sh
comping -p "这个仓库是干什么的？"
```

## 命令

| 命令 | 作用 |
|---|---|
| `/help` | 列出命令与快捷键 |
| `/model [id]` | 查看或切换模型 |
| `/provider [名字]` | 查看或切换 provider |
| `/status` | 会话、用量、上下文占用 |
| `/compact` | 压缩早期对话，腾出上下文 |
| `/clear` | 清空历史另起会话（旧记录可 `-r` 找回） |
| `/init` | 在项目里生成 `.agent/config.toml` |
| `/exit` | 退出 |

以 `/` 开头的一律是本地命令，不会发给模型；打错会明确报错而不是漏给模型。

## 按键

| 键 | 作用 |
|---|---|
| `Enter` / `Shift-Enter` | 发送 / 换行 |
| `Esc` | 中断当前请求，再按退出 |
| `Ctrl-C` | 退出 |
| `Tab` | 在输入框与对话区之间切焦点 |
| `↑` `↓` | 滚动；对话区焦点下选中工具条目 |
| `空格` / `Enter` | 展开选中的工具输出 |
| `y` `n` `a` | 批准面板：允许一次 / 拒绝 / 本轮全部允许 |

## 配置

用户级 `~/.config/comping/config.toml`，项目级 `.agent/config.toml` 覆盖同名键
（项目级只需写要改的，没写的键回落到用户级）。也可以用 `comping /init` 生成模板。

```toml
[agent]
max_steps = 25              # 单次请求最多几轮，防死循环
tool_concurrency = 4
tool_timeout_secs = 300     # 单个工具超时兜底，0 = 不限
compact_at = 0.85           # 上下文占用到这个比例就压缩
keep_recent = 12
bash_allowlist = ["cargo", "git"]   # 首个词匹配，免确认
auto_approve_tools = []     # 永久免确认的工具
disabled_tools = []         # 彻底禁用的工具
```

`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` 都能替代配置里的 `api_key`。

## 命令行

```
comping [选项] [问题]

  -p, --prompt <文本>    提问后退出
  -r, --resume <id>      恢复会话（用 -l 列出）
  -l, --list-sessions    列出会话
      --provider <名字>   覆盖配置
      --model <id>       覆盖配置
      --cwd <路径>        工作目录
  -y, --yes              所有工具免确认（谨慎）
```

## 架构

```
crates/
  agent-core/   会话状态机、工具注册与并行执行、取消与超时。不含 IO 假设
  llm/          协议适配器（Anthropic / OpenAI / Gemini）+ SSE 解析 + 重试
  tools/        内置工具实现 + 工作区沙箱
  session/      TOML 配置、JSONL 会话持久化
  prompt/       系统提示词与 slash 命令（零依赖）
  tui/          Ratatui 前端，产出可执行文件 comping
  mock-provider/ 可脚本化的假模型服务，仅开发用
```

分层约定：`agent-core` 只产出 `AgentEvent`，不碰网络也不碰渲染。同一个事件流
可以喂给 TUI、headless 模式，将来接 Web 前端也不必重写内核。

`AgentEvent::ApprovalNeeded` 在并行工具时会连发多个，消费方**必须排队**——
丢掉任何一个都会让那个工具的 respond 被 drop，进而被误判成用户拒绝。

## 开发

```sh
cargo test                      # 146 个测试
cargo build --release           # 单二进制
```

三个协议适配器的集成测试全部打在 `mock-provider` 上，不需要 API key：

```sh
cargo run -p mock-provider -- --port 8977 --protocol anthropic --scenario tool
curl -N -X POST http://127.0.0.1:8977/v1/messages \
  -H 'anthropic-version: 2023-06-01' -H 'x-api-key: k' \
  -d '{"model":"m","messages":[],"stream":true}'
```

mock 的工具参数是**分片下发**的，所以"边解析边拼接"的路径真的被跑到；
响应也用 chunked 逐事件 flush，一次性吐完的话分片拼接的 bug 是测不出来的。

注意 mock 的保真度上限就是我们对协议的理解上限——它能验证 SSE 切分、index 归并、
角色映射、缓存用量归一这类结构性逻辑，验证不了真实 API 的字段细节。

## 边界

- 只能操作工作区内的路径，工具会拒绝越界访问
- 破坏性操作执行前会要求确认；`--yes` 会关掉这个确认，只剩沙箱
- 上下文预算是按字符数估算的，不是真分词器
