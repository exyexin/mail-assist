# 截止时间漏记 Bad-case 分析（需求 4）

> 数据来源：`data/mail2.db`（只读查询，未做任何写入）；分析日期：2026-08-28。
> 结论同时覆盖需求 2（通知/反馈式邮件不建待办）与需求 3（含“面试”关键字但非预约的邮件不建待办），
> 因为它们在真实数据中是同源的。

## 1. 分析方式

用 sqlite3 对真实库执行 6 组只读查询：

| 查询 | 内容 |
|---|---|
| Q1 | 正文含时限表达（小时内/内完成/24H/48H/72H）的邮件 与 其事项的 deadline |
| Q2 | `agent_runs` 轨迹中 `create_item(…"deadline": null…)` 的历史记录 |
| Q3 | 分类为 interview/written_test/assessment/todo 且主题含 问卷/调研/反馈/结果/投递/感谢/offer/录用 的邮件 |
| Q4 | `items.deadline IS NULL` 统计 |
| Q5 | 邮件分类分布 |

## 2. Bad case 清单（真实证据）

### A. 时限表达明确，但事项 deadline = NULL（Q1）

| 邮件 | 主题 | 分类 | 事项 | deadline | needs_review |
|---|---|---|---|---|---|
| 74 | 【京东校招】JD YOUNG - 实习生计划测评通知（正文：“建议您在**48小时内**完成测评”） | assessment | 25 | NULL | 0 |
| 191 | 【京东校招】2027 JDS测评通知（正文：“建议您在**48小时内**完成测评”） | assessment | 131 | NULL | 0 |
| 193 | 【快手校园招聘】在线人才测评邀请（正文：“请在**7个工作日之内**完成在线测评”） | assessment | 133 | NULL | 0 |
| 192 | 【快手科技】邀请你参加面试，请预约你的面试时间 | interview | 132 | NULL | 0 |
| 175 | 小米集团测评邀请 | assessment | 116 | NULL | 1 |
| 197 | 【Shopee】校园招聘简历更新邀请 | todo | 137 | NULL | 0 |
| 39 | 黄大年茶思屋认证（正文：“链接有效期为**30天**，请在有效期内完成认证”） | notification | 9 | NULL | 0 |

对照：同批 101/115 两封测评邮件（“请在X小时内完成”）**有** deadline（`2026-04-14T17:06:00+08:00` 等），
说明旧流水线 legacy 路径的推算只对部分邮件生效——凡由 Agent 建项的都漏了。

### B. Agent 建项一律 deadline=null（Q2）

`agent_runs` 轨迹中 `create_item` 全部为 `"deadline": null`，包括：

- 华为茶思屋“请在30天内点击链接完成邮箱认证”（run 38）→ 事项无截止时间；
- 东航“今天的展示”（run 52）、导师指示 rk3588（run 8）→ 无明确时间，可接受；
- 大量 ICLR/Notion/GitHub/Google 学术 验证码/验证类邮件被建成 **notification 事项**（runs 2–65）——
  按新规则（通知类不建项）这些本不该存在。

### C. 反馈式/投递类邮件被归入待办分类并建项（Q3）

| 邮件 | 主题 | 被归为 | 建了事项 |
|---|---|---|---|
| 100/122/185 | 【米哈游miHoYo】岗位投递邀请 | todo | 47/67/125 |
| 103/104/105/106/112 | 【孙颢城】恭喜您通过初筛！开启专属投递及AI面试之旅 | interview | 50/51/52/53/59 |
| 132 | 淘天集团-算法岗位暑期实习-诚邀投递简历 | todo | 77 |
| 148 | 【百度】2027校园招聘邀请您投递 | todo | 91 |
| 180/181/188 | 拼多多在线笔试邀请（真实笔试，分类正确） | written_test | 121/122/128 |

前 4 行即“含面试/投递关键字但并非预约面试”的典型误判（需求 3）。

### D. 存量统计（Q4/Q5）

- `kind=todo AND deadline IS NULL`：**31 个 active**（其中很多是 Agent 建的、`needs_review=0`，界面不提示）；
- `kind=notification`：**62 个 active**（新规则下不再产生）；
- 分类分布：notification 65 / interview 36 / misc 35 / conversation 23 / todo 15 / written_test 13 / assessment 10。

## 3. 根因（代码路径）

1. **Agent 建项后跳过一切推算（主因，bad case A/B）**：
   `pipeline.rs ensure_item` 开头 `if db.item_for_email(email_id)?.is_some() { return Ok(()); }`
   ——Agent 已用 `create_item` 建项（`deadline:null`）时，相对时限推算与时间归一化全部被跳过。
2. **非 RFC3339 明确时间被丢弃**：`create_item`/`extract_todo` 只认 RFC3339，
   `2026-04-24 11:00(GMT+08:00)`、`2026-04-24 11-00` 这类写法被静默置空。
3. **相对时限正则覆盖不足（本次分析新发现）**：
   - “**建议您**在48小时内完成测评”——无“请/请在”前缀，旧正则不匹配；
   - “请在7个**工作日之**内完成在线测评”——“之”夹在“日”和“内”之间，旧正则不匹配；
   - “24H / 24h / 72hr / within 48 hours”英文单位完全不支持（用户报告的原话）。
4. **推算基准缺失**：旧 `infer_relative_deadline` 只用 `sent_at`，无 Date 头即放弃。
5. **Agent 建的 todo 无 deadline 时 `needs_review` 恒为 false**：界面不提示“待补截止时间”。
6. **反馈式邮件守卫缺失（需求 2/3 同源）**：Agent 提示词只强调“面试/测评/笔试建项”，
   没有反馈式邮件的反例约束，也没有确定性兜底。

## 4. 修复对照

| # | 修复 | 位置 |
|---|---|---|
| 1 | Agent 建项后统一补推算：`fill_missing_deadline`（LLM 值 → 明确时间归一化 → 相对时限；基准 sent_at 优先、received_at 兜底；补 `remind_at`/`notes`；推算不出则 `needs_review=true`） | `src/pipeline.rs` |
| 2 | 人类可读时间归一化 `parse_human_datetime`（`2026-04-24 11:00(GMT+08:00)`、`2026/4/24 14:00`、`4月24日 下午2:00`、`截止2026-09-30`、`请于9月30日前` 等） | `src/deadline.rs` |
| 3 | 相对时限正则：去掉前缀要求（覆盖“建议您在48小时内”）、支持“之”内（“7个工作日之内”）、支持 `H/h/hr/HR/hours`、`within N hours/days` | `src/deadline.rs` |
| 4 | `infer_relative_deadline` 增加 received_at 兜底 | `src/deadline.rs` |
| 5 | `create_item`/`update_item` 工具对 deadline 做宽容归一化后再校验 | `src/agent/tools.rs` |
| 6 | 严格建项规则：仅 {interview, written_test, assessment} 或 有明确/推算截止时间 才建待办；通知类一律不建 | `src/pipeline.rs` |
| 7 | 反馈式邮件守卫：主题命中 问卷/调研/结果/投递成功/感谢信/通过初筛 等且无预约/时间标记 → 强制归 notification 且撤销同批误建待办 | `src/pipeline.rs` |
| 8 | Agent/LLM 提示词：反馈式邮件反例 + deadline 写法转换说明 | `src/agent/mod.rs`, `src/llm.rs` |
| 9 | 重分类为通知类不再新建通知事项 | `src/api.rs` |
| 10 | 存量库迁移：`categories` 表 notification 分类 `create_item=0` | `src/db.rs` |

## 5. 验证

- `cargo test`：**39 单测 + 19 e2e 全绿**；新增用例覆盖：
  - Agent `create_item`（deadline=null）→ 依据“请在24H内完成”补填 deadline（e2e_agent_created_item_gets_relative_deadline_filled）；
  - “面试结果通知/面试问卷”不建待办（e2e_feedback_email_with_interview_keyword_not_todo）；
  - “面试时间：2026-04-24 11:00(GMT+08:00)”归一化（e2e_explicit_human_time_normalized_to_deadline）；
  - 会议类无截止时间不建待办（e2e_generic_todo_without_deadline_not_created）；
  - 重分类为通知不建项（e2e_reclassify_to_notification_no_item）；
  - 标注 API（e2e_email_label_api）；
  - 正则单测：`建议您在48小时内`、`7个工作日之内`、`24H`、`within 48 hours`、`截止2026-09-30` 等。
- 界面（headless chromium DOM 断言 17 项全 PASS + 截图见 `docs/evidence/`）：
  待办/通知两个独立页面、邮件列表标注列/徽标/过滤、标注弹窗、邮件详情标注展示，均无 JS 错误。

## 6. 存量数据处理建议（本次未自动执行）

- 旧库中 31 个无 deadline 的 todo 与 62 个 notification 事项为历史遗留，未自动删除；
  可在界面手动完成/静默/删除，或对 needs_review=1 的补截止时间。
- 可后续增加“按标注导出训练集”，把 `emails.user_label/user_note` 用于提示词迭代。
