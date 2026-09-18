# mail2 — 邮件管理工具

一个运行于任意 Linux 发行版的邮件管理工具：**IMAP 收信 → DeepSeek 大模型分类 → 待办/DDL 提醒 → 定时检查与失败重试 → Web 控制台（前后端分离）**。

## 功能

1. **收发邮件**：IMAP（SSL 993 / 明文）收信 + SMTP（SSL 465 / STARTTLS 587 / 明文）发信。
2. **DeepSeek 分类**：每封新邮件调用 LLM 分类：
   - 事务预约/待办（`todo`）
   - 面试 / 笔试 / 测评（`interview` / `written_test` / `assessment`）
   - 通知类（`notification`，含投递成功/问卷调研/结果通知等反馈式邮件）
   - 招聘推广（`career_promo`，宣讲会/双选会/网申推荐/投递邀请等群发）
   - 对话交流型（`conversation`）、其他（`misc`）
   - 类型可在 `llm.yaml` 中增删改；Agent 运行期也可用 `create_category` 新建，
     **新建分类默认不建项**（`create_item=false`），且必须落库校验 id/label（不允许把中文 label 当 id）。
   - 反馈式邮件即使出现“面试/笔试/测评”字样（如“面试体验问卷”“面试结果通知”），
     只要不是预约/安排一次面试笔试测评，就归为通知类，**不会**创建待办。
3. **待办处理**（只有“需要本人亲自行动”的邮件才建待办）：
   - **自动建待办仅限 面试/笔试/测评，以及含明确截止时间/时限的预约、材料提交类邮件**
     （如“请在24H内完成”“截止9月30日”“链接24小时后失效”）；
   - **宣讲会/双选会/网申推荐/投递邀请等招聘推广邮件只做通知，永不建待办**（确定性守卫 + Agent 提示词双重约束，
     Agent 若已误建会立即撤销）；
   - 通知类/反馈式邮件不建事项；
   - LLM 提取结构化信息：公司/个人、事件（5-10 字）、截止时间 DDL；
   - 截止时间支持三类来源：LLM 提取的 RFC3339、正文人类可读时间自动归一化
     （如 `2026-04-24 11:00(GMT+08:00)` → RFC3339）、相对时限推算（发件/收件时间 + `X小时/日内`）；
     相对时限还会与模型给出的时间做一致性校验，纠正“把 UTC 墙钟当本地时间”的 8 小时偏差；
   - **同一件事（同公司 + 同类型 + 同一场次）的多封邮件合并为一条待办，以最新邮件为准**；
     不同场次（截止时间相差 >24h）保留为独立待办；
   - 提醒策略分级（`remind_policy`）：
     - 常规（中长期）→ 截止前 `days_before` 天 `lead_time`（默认 09:00）提醒一次；
     - 紧急（收到时距截止 ≤24h）→ **立即提醒 + 截止前 2 小时**再提醒；
     - 链接/资格失效类（“链接 N 小时后失效”“有效期 N 天”）→ 立即提醒 + 截止前 2 小时；
   - 提醒邮件主题严格为 `【类型/{公司,个人}/事件/截止时间月日】`，例如 `【面试/字节跳动/技术面试/0910】`；
   - 可选向 App 推送（通用 HTTP webhook，如 server酱/企业微信机器人）；
   - 回复提醒邮件包含 `【已完成】` / `【不再提醒】`（或英文关键词）→ 事项自动标记完成/静默，不再提醒；
   - 面试/笔试/测评类邮件无明确截止时间时 → 事项标记“待补截止时间”，可在 Web 界面补全。
4. **检查功能**：每次收到新邮件时 + 每天 `06:00 / 12:30 / 18:30 / 00:00` 检查：
   - 按提醒策略生成一个或多个提醒时刻（同一事项两次提醒间隔 <30 分钟时只发一封，避免轰炸）；
   - 漏发补偿（应提醒而未发成功 → 立即补发）；
   - 发送失败 → 记录错误并 **延迟 30 分钟重试**；
   - 已完成/静默事项永不提醒。
5. **Web 界面**（原生 JS SPA，无构建，前后端通过 REST 分离）：
   - **待办 / 通知两个独立页面**（不再混排），事项增删改查、完成/静默/立即提醒；
   - **事项与邮件列表统一按邮件收件时间倒序**（无关联邮件的手动事项按创建时间兜底）；
   - 事项行可一键**跳转查看关联邮件**（邮件详情弹窗，含重新分类与标注），方便对照；
   - 邮件列表支持**手动标注**（分类正确/分类错误/应建待办/不应建待办/漏截止时间 + 备注），
     可按标注过滤，用于后续改进分类与建待办规则。
6. **控制 API**：`/api/v1` 版本化、统一信封 `{code,message,data}`、Bearer 鉴权（可选）、错误语义化——便于其他客户端接入与扩展。

## 快速开始

```bash
# 依赖：Rust（cargo）、OpenSSL（TLS）；Web 无任何构建依赖
cargo build --release            # 产物 target/release/mail2
./target/release/mail2 init      # 生成 config.example / llm.yaml.example

# 准备配置（二选一，见下）
cp config.example config.yaml    # 或直接使用邮箱服务商导出的文本 config
cp llm.yaml.example llm.yaml     # 填入 DeepSeek api_key/model

./target/release/mail2 serve     # 启动：IMAP 轮询 + 调度器 + Web/API（默认 127.0.0.1:8080）
```

打开浏览器访问 `http://127.0.0.1:8080` 进入控制台。

### 邮箱配置（`config`）

支持两种形态，启动时优先读 `config.yaml`，其次读 `config`：

1. **结构化 YAML**（`config.yaml`，推荐，字段见 `config.example`）：
   ```yaml
   listen: "127.0.0.1:8080"
   auth_token: ""            # API 令牌，空 = 不鉴权
   data_dir: "./data"        # 数据目录（日志 + 数据库默认位置）
   mail_db: ""               # 邮件库路径，空 = data_dir/mail.db
   todo_db: ""               # 待办库路径，空 = data_dir/todo.db
   timezone: "Asia/Shanghai" # 空 = 系统本地
   poll_interval_secs: 60
   reminder:
     to: ""                  # 提醒目标邮箱，空 = 本人邮箱
     days_before: 1          # 截止前 N 天提醒
     lead_time: "09:00"      # 提醒当天几点发送
     webhook_url: ""         # 可选 App 推送 webhook
   check_times: ["06:00", "12:30", "18:30", "00:00"]
   mail:
     address: "you@example.com"
     imap: { host: "imap.example.com", port: 993, user: "you@example.com", password: "***" }
     smtp: { host: "smtp.example.com", port: 465, user: "you@example.com", password: "***" }
   ```
2. **服务商文本导出**（`config`）：直接把邮箱服务商"客户端专用密码/配置参数"页面导出的文本
   粘贴进来即可，程序自动正则提取 邮件地址 / IMAP(SMTP) 服务器与 SSL 端口 / 专用密码。
   例如 XMU 的导出页格式（"邮件地址\txxx"、"收信服务器 (IMAP)\t...\tSSL 端口: 993"）。

> **证书域名不匹配**：部分服务商证书与实际域名不一致（如 `*.icoremail.net` 服务于
> `imap.stu.xmu.edu.cn`），严格校验会握手失败。此时在该端点上加 `tls_insecure: true`
> （加密通道保留，仅跳过域名校验）。默认 `false` 严格校验。

> **不改动邮箱状态**：收信使用 `UID FETCH (BODY.PEEK[])` 只读拉取，**不会**设置 `\Seen`，
> 因此程序读过的邮件在你的邮箱/客户端里仍保持未读；也不会删除、移动或改动任何标志位。

### LLM 配置（`llm.yaml`）

```yaml
api_key: "sk-xxx"
base_url: "https://api.deepseek.com"
model: "deepseek-chat"      # 或 deepseek-v4-pro 等
timeout_secs: 60
agent:
  enabled: true             # tool call 模式（失败自动降级旧固定 prompt 路径）
  max_tool_rounds: 8
categories:                 # 分类类型可增删改（首次启动种子）
  - { id: todo,          label: 事务预约/待办, create_item: true }
  - { id: interview,     label: 面试,         create_item: true }
  - { id: assessment,    label: 测评,         create_item: true }
  - { id: written_test,  label: 笔试,         create_item: true }
  - { id: notification,  label: 通知类,       create_item: false }
  - { id: career_promo,  label: 招聘推广,     create_item: false }   # 宣讲会/双选会/网申推荐
  - { id: conversation,  label: 对话交流型,   create_item: false }
  - { id: misc,          label: 其他,         create_item: false }
```
`create_item: true` 的类别会自动建立待办；Agent 运行期新建的分类**默认 `create_item=false`**
（需要建项时由 Agent 显式调用 `create_item` 工具）。

> ⚠️ `config` / `config.yaml` / `llm.yaml` 含密钥，已被 `.gitignore` 排除，请勿提交仓库。

## 命令行

```
mail2 serve                        # 启动服务（IMAP 轮询 + 调度 + Web/API）
mail2 fetch-once                   # 拉取并处理一次新邮件
mail2 fetch-once --limit 15 --no-check   # 只处理最新 15 封且不触发检查（测试真实邮箱：零发送）
mail2 check-once                   # 执行一次待办检查（漏发/失败重试）
mail2 init                         # 生成配置模板
```

> `--limit N`：只处理最新 N 封（UID 游标不推进，旧邮件不会被跳过）；`--no-check`：收信后
> 不触发检查器（**保证不发生任何 SMTP 发送**）——用真实邮箱测试时请务必加上。

## 日志（追溯）

日志同时输出到**控制台**与**按天轮转的文件**（`<data_dir>/logs/mail2.log.YYYY-MM-DD`，默认
`./data/logs/`），用于追溯邮件处理全过程：

- **邮件拉取进度**：IMAP 登录/SELECT、UID 范围、拉取数量与最大 UID、耗时；每封邮件的处理进度 `i/N`（主题/发件人/message-id）；
- **LLM Agent 调用流程**：逐轮记录（轮次/消息数）、每次调用的完整请求与响应内容、耗时、`finish_reason`、工具调用参数与返回结果、最终 JSON；
- **处理结果**：每封邮件的分类与建项结果（item/deadline/remind_at）、Agent 运行汇总（rounds/tool_calls/审批）、每批次 RunReport 汇总（fetched/new/classified/reminders 等）、检查器逐项决策与发送结果。

级别由 `RUST_LOG` 控制（默认 `info`）：

```bash
RUST_LOG=info  ./mail2 serve        # 默认：拉取/分类/建项/发送等关键结果
RUST_LOG=debug ./mail2 serve        # 详细：LLM 完整请求/响应、工具调用参数与结果、逐项检查决策
RUST_LOG=trace ./mail2 serve        # 更细：IMAP 每封消息 UID/大小、调度 tick 明细
```

> 提示：`debug` 级别会混入依赖库（rustls 等）的日志，只想看本程序时可加模块前缀过滤，
> 例如 `RUST_LOG=mail2=debug`（`tracing` 子系统名均为 `mail2::*`）。

> ⚠️ `debug` 及以上级别会把邮件正文、LLM 请求/响应全文写入日志文件（便于追溯，但含隐私内容）；
> **IMAP/SMTP 密码与 LLM api_key 任何级别都不会写入日志**。

## 控制 API（/api/v1）

统一信封：`{"code":0,"message":"ok","data":…}`；错误：HTTP 4xx/5xx + `{"code":<状态码>,"message":"…"}`。
鉴权：`config.auth_token` 非空时需 `Authorization: Bearer <token>`（`/api/v1/health` 除外）。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/v1/health` | 健康检查 |
| GET | `/api/v1/config` | 运行配置（密钥脱敏为 `***`） |
| GET | `/api/v1/items?kind=&status=&category=&q=&limit=` | 事项列表 |
| POST | `/api/v1/items` | 新建事项 `{kind,title,party,event,category,deadline(RFC3339),notes}` |
| GET | `/api/v1/items/{id}` | 事项详情 |
| PUT | `/api/v1/items/{id}` | 更新事项（字段同上） |
| DELETE | `/api/v1/items/{id}` | 删除事项及其发送日志 |
| POST | `/api/v1/items/{id}/complete` | 标记完成 |
| POST | `/api/v1/items/{id}/silent` | 标记静默（不再提醒） |
| POST | `/api/v1/items/{id}/remind-now` | 立即发送一次提醒（手动） |
| GET | `/api/v1/emails?category=&label=&q=&limit=` | 邮件列表（`label=__none__` 查未标注） |
| GET | `/api/v1/emails/{id}` | 邮件详情 |
| POST | `/api/v1/emails/{id}/reclassify` | 手动重分类 `{category}`（todo 会重新提取并创建/更新事项；重分类为通知类不建事项） |
| PUT | `/api/v1/emails/{id}/label` | 保存/清除标注 `{label, note}`（label 空 = 清除；预设值见 Web 界面） |
| POST | `/api/v1/checker/run` | 手动触发一次检查 |
| GET | `/api/v1/logs?item_id=&status=&limit=` | 发送日志 |

**扩展方式**：新增资源 = 新增路由模块 + 数据表，保持版本前缀与信封约定不变。

## 本地测试（不触碰真实邮箱 / 不消耗真实 API）

```bash
./scripts/dev.sh greenmail-start   # 本地邮件服务器（greenmail: SMTP 3025 / IMAP 3143，auth 禁用）
./scripts/dev.sh test              # cargo test：单元 + 端到端（mock LLM + FakeClock 注入时间）
./scripts/dev.sh mock-llm          # 可选：离线 mock LLM（127.0.0.1:18765），手动联调用
```

端到端测试覆盖：收信分类 → 待办提取 → 提前 1 天精确主题提醒 → 回复"已完成/不再提醒"静默 →
发送失败延迟 30 分钟重试 → 每日 4 个检查时刻触发 → 通知/对话/其他分类 → 重复邮件去重 →
无截止时间兜底 → REST API 增删改查。

手动联调（不想消耗真实 DeepSeek 额度）：把 `llm.yaml` 的 `base_url` 改为
`http://127.0.0.1:18765`，配合 `mock-llm` 即可完整跑通收发邮件全流程。

## 部署（systemd）

```ini
# /etc/systemd/system/mail2.service
[Unit]
Description=mail2 mail manager
After=network-online.target

[Service]
Type=simple
User=youruser
WorkingDirectory=/opt/mail2
ExecStart=/opt/mail2/mail2 serve --config-dir /opt/mail2
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now mail2
```

## 目录结构

```
mail2/
├── src/               # Rust 后端（config/db/mail/llm/pipeline/scheduler/checker/notifier/api）
├── web/               # 前端 SPA（index.html + app.js + style.css，无构建）
├── tests/             # e2e 测试（greenmail + mock LLM + FakeClock）
├── scripts/dev.sh     # 本地邮件服务器 / mock LLM / 运行脚本
├── config.example     # 邮箱配置模板（YAML 形态）
├── llm.yaml.example   # LLM 配置模板
└── data/              # 运行时生成：mail.db + todo.db + logs/（按天轮转日志）
```

### 数据存储：邮件库 / 待办库分离

| 文件 | 内容 | 说明 |
|---|---|---|
| `data/mail.db` | `emails`（邮件原文/分类/标注）、`accounts`（收发信账户）、`agent_runs`（LLM 处理轨迹）、`kv`（收信游标、调度标记） | 相当于"邮件语料 + 收信状态"，可从 IMAP 重新拉取 |
| `data/todo.db` | `items`（待办/事项）、`send_log`（提醒发送记录）、`approvals`（Agent 改删审批）、`categories`（分类全集） | 你真正在意的数据，可单独备份/迁移 |

- 两库之间只保留整数引用（`emails.item_id` ↔ `items.source_email_id`），不做外键约束；
  待办按"关联邮件收件时间"排序在应用层完成，因此不需要跨库 JOIN。
- 路径可由 `config.yaml` 的 `mail_db` / `todo_db` 指定（留空 = `data_dir` 下默认文件名）。
- **旧版单库自动拆分**：首次启动时若发现旧的 `data/mail2.db` 且两个新库都不存在，
  会按上表把数据分别复制到 `mail.db` / `todo.db`，并把旧文件改名为 `data/mail2.db.bak` 保留（不删除）。

## 安全说明

- 邮箱专用密码与 LLM key 仅存放于 `config`/`llm.yaml`（不入库、不打日志、API 返回脱敏）；
- 默认仅监听 `127.0.0.1`；如需对外提供控制台请设置 `auth_token` 并配合反向代理 TLS；
- 提醒邮件正文会提示收件人如何回复静默，回复识别按 Message-ID/主题匹配，防止误伤普通对话邮件。

## 技术栈

Rust（tokio / axum / async-imap / lettre / rusqlite / reqwest / chrono-tz / clap）+ 原生 JS SPA。
单二进制、无运行时依赖，适用于任何带 glibc 的 Linux 发行版。
