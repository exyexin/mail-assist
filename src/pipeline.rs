//! 新邮件处理流水线：
//! IMAP 拉取（UID 增量 / 手动范围）→ 去重入库（含发件时间、账户）→ 提醒回复识别
//! → LLM Agent 分类与建项（失败降级旧固定 prompt）→ 相对截止时间推算
//! → 更新 UID 游标 → 触发一次检查（需求：每次收到新邮件时检查待办完成情况）。
//!
//! 手动范围拉取（opts.since）：拉取全量后在本地按发件时间过滤，不推进 UID 游标，
//! 由正常轮询补收其余邮件（message_id 去重防重复）。

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::agent;
use crate::checker;
use crate::clock::Clock;
use crate::config::AppConfig;
use crate::db::{self, Db};
use crate::llm::LlmClient;
use crate::mail::{parse_mail, ImapClient, SmtpClient};
use crate::model::{EmailRecord, Item, ItemKind, ItemStatus, ReplyIntent, RunReport, TodoExtract};
use crate::notifier;

/// 处理选项（测试/运维场景）：
/// - limit: 只处理最新 N 封（游标不推进，未处理的旧邮件不会被永久跳过）
/// - run_check: 是否在收信后触发检查器（测试真实邮箱时设为 false，避免任何 SMTP 发送）
/// - since: 手动范围拉取的起始时间（按发件时间过滤；None = 常规增量拉取）
/// - advance_cursor: 是否推进 UID 游标（手动范围拉取不推进）
#[derive(Debug, Clone, Copy)]
pub struct ProcessOpts {
    pub limit: Option<usize>,
    pub run_check: bool,
    pub since: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub advance_cursor: bool,
}

impl Default for ProcessOpts {
    fn default() -> Self {
        ProcessOpts {
            limit: None,
            run_check: true,
            since: None,
            advance_cursor: true,
        }
    }
}

/// 处理一批新邮件（默认账户；兼容旧调用）。
pub async fn process_new_mails(
    db: &Db,
    imap: &ImapClient,
    smtp: &SmtpClient,
    llm: &LlmClient,
    cfg: &AppConfig,
    clock: &dyn Clock,
) -> Result<RunReport> {
    let account_id = db.default_account_id()?.unwrap_or(1);
    process_new_mails_opts(db, imap, smtp, llm, cfg, clock, account_id, ProcessOpts::default())
        .await
}

/// 处理一批新邮件（指定账户 + 选项；见 [ProcessOpts]）。
pub async fn process_new_mails_opts(
    db: &Db,
    imap: &ImapClient,
    smtp: &SmtpClient,
    llm: &LlmClient,
    cfg: &AppConfig,
    clock: &dyn Clock,
    account_id: i64,
    opts: ProcessOpts,
) -> Result<RunReport> {
    let mut report = RunReport::default();
    let started = std::time::Instant::now();

    // 手动范围拉取：全量拉取后本地过滤；常规：UID 增量。
    let (mut raws, max_uid) = if opts.since.is_some() {
        match imap.fetch_new(None).await {
            Ok(v) => v,
            Err(e) => {
                warn!("IMAP 全量拉取失败（本次跳过）: {e:#}");
                return Ok(report);
            }
        }
    } else {
        let last_uid: Option<u32> = db
            .kv_get(&format!("last_uid:{account_id}"))?
            .or(db.kv_get("last_uid")?)
            .and_then(|v| v.parse().ok());
        info!("账户 {account_id} 增量拉取: last_uid={last_uid:?}");
        match imap.fetch_new(last_uid).await {
            Ok(v) => v,
            Err(e) => {
                warn!("IMAP 拉取失败（本次跳过，稍后轮询重试）: {e:#}");
                return Ok(report);
            }
        }
    };
    report.fetched = raws.len();
    info!(
        "账户 {account_id} 拉取到 {} 封新邮件 max_uid={max_uid:?} 耗时={}ms",
        raws.len(),
        started.elapsed().as_millis()
    );
    // 限流：只处理最新 N 封（IMAP UID 升序 → 取尾部）
    if let Some(limit) = opts.limit {
        if raws.len() > limit {
            let keep = raws.len() - limit;
            raws = raws.split_off(keep);
            info!("限流模式: 仅处理最新 {limit} 封（共拉到 {} 封）", raws.len() + keep);
        }
    }

    let total = raws.len();
    for (i, raw) in raws.iter().enumerate() {
        let parsed = match parse_mail(raw) {
            Ok(p) => p,
            Err(e) => {
                warn!("邮件解析失败: {e:#}");
                continue;
            }
        };
        debug!(
            "处理邮件进度 {}/{}: subject={} from={} message_id={}",
            i + 1, total, parsed.subject, parsed.from_addr, parsed.message_id
        );
        // 手动范围拉取：按发件时间过滤。无法解析发件时间的邮件保留导入
        // （否则会静默丢失；入库后由 message_id 去重，不会重复处理）。
        if let Some(since) = opts.since {
            let sent = chrono::DateTime::parse_from_rfc3339(&parsed.sent_at).ok();
            match sent {
                Some(dt) if dt < since => {
                    debug!(
                        "范围过滤跳过（早于起始时间）: subject={} sent_at={}",
                        parsed.subject, parsed.sent_at
                    );
                    continue;
                }
                None => {
                    debug!("范围拉取保留（发件时间未知）: subject={}", parsed.subject);
                }
                _ => {}
            }
        }
        let now = clock.now().to_rfc3339();
        let email = EmailRecord {
            id: 0,
            uid: None,
            message_id: parsed.message_id.clone(),
            subject: parsed.subject.clone(),
            from_addr: parsed.from_addr.clone(),
            from_name: parsed.from_name.clone(),
            body_text: parsed.body_text.clone(),
            category: String::new(),
            sent_at: parsed.sent_at.clone(),
            received_at: now,
            item_id: None,
            reply_to_item_id: None,
            account_id,
            handled: false,
            user_label: String::new(),
            user_note: String::new(),
        };
        // ---- 0. 跳过我们自行发出的邮件（如提醒邮件被收件箱回显），防止误判为回复 ----
        if db
            .find_send_log_by_message_id(&parsed.message_id)?
            .is_some()
            || (parsed.subject.starts_with("【")
                && parsed.subject.ends_with("】")
                && parsed.from_addr == cfg.mail.address)
        {
            info!("跳过自发邮件: {}", parsed.subject);
            continue;
        }

        let Some(email_id) = db.insert_email(&email)? else {
            debug!("跳过重复邮件（message_id 已处理）: {}", parsed.message_id);
            continue; // 已处理过（message_id 去重）
        };
        report.new_emails += 1;

        // ---- 1. 提醒回复识别（In-Reply-To / References / 主题匹配） ----
        if let Some((item_id, intent)) = detect_reply(db, llm, &parsed).await.unwrap_or(None) {
            if intent == ReplyIntent::None {
                debug!("回复匹配到提醒但意图不明（不处理）: item={item_id} email={email_id}");
            }
            let new_status = match intent {
                ReplyIntent::Done => ItemStatus::Completed,
                ReplyIntent::Silent => ItemStatus::Silent,
                ReplyIntent::None => ItemStatus::Active,
            };
            if intent != ReplyIntent::None {
                db.set_item_status(item_id, new_status)?;
                report.items_updated += 1;
                report.replies_handled += 1;
                info!(
                    "提醒回复处理: item={} status={} from={}",
                    item_id,
                    new_status.as_str(),
                    parsed.from_addr
                );
            }
        }

        // ---- 2. 分类 + 建项：Agent 优先，失败降级旧路径 ----
        let mut category: Option<String> = None;
        let mut agent_item: Option<serde_json::Value> = None;
        if cfg.llm.agent.enabled {
            match agent::run_agent(llm, db, cfg, clock, email_id).await {
                Ok(out) => {
                    report.tool_rounds += out.rounds;
                    report.approvals_created += out.approvals_created;
                    info!(
                        "agent 完成 email={} rounds={} tools={} approvals={}",
                        email_id, out.rounds, out.tool_calls, out.approvals_created
                    );
                    let cat = out
                        .category
                        .clone()
                        .filter(|c| db.get_category(c).map(|x| x.is_some()).unwrap_or(false));
                    category = cat;
                    agent_item = out.item.clone();
                }
                Err(e) => {
                    warn!("Agent 处理失败，降级旧路径 email={email_id}: {e:#}");
                }
            }
        }
        if category.is_none() {
            // ---- 降级：旧分类（失败降级 misc，邮件不丢） ----
            let c = match llm
                .classify(&parsed.subject, &parsed.body_text, &parsed.from_addr)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    warn!("LLM 分类失败，降级 misc: {e:#}");
                    report.llm_fallback_misc += 1;
                    crate::model::CATEGORY_MISC.to_string()
                }
            };
            category = Some(c);
        }
        let mut category = category.unwrap_or_else(|| crate::model::CATEGORY_MISC.to_string());
        report.classified += 1;

        // ---- 2.5 反馈式邮件守卫（需求 3）：主题命中反馈标记（问卷/结果/投递通知等）且无预约/时间
        //      标记时，即使 Agent/LLM 归为面试类也不建待办，统一归 notification ----
        if is_feedback_email(&email.subject) && !subject_has_time(&email.subject) {
            if CORE_TODO_CATEGORIES.contains(&category.as_str())
                || category == crate::model::CATEGORY_TODO
            {
                info!(
                    "反馈式邮件不建待办（重分类为 notification）: email={email_id} category={category} subject={}",
                    email.subject
                );
                category = crate::model::CATEGORY_NOTIFICATION.to_string();
                db.update_email_category(email_id, &category)?;
                // 撤销同一批次中 Agent 刚创建、无截止时间的待办
                if let Ok(Some(item_id)) = db.item_for_email(email_id) {
                    if let Ok(Some(it)) = db.get_item(item_id) {
                        if it.kind == ItemKind::Todo && it.deadline.is_none() {
                            info!("撤销反馈邮件误建待办 item={item_id} email={email_id}");
                            let _ = db.delete_item(item_id);
                        }
                    }
                }
            }
        }
        // ---- 2.6 招聘推广守卫（需求 1）：宣讲会/双选会/网申推荐/投递邀请等群发推广，
        //      一律只做通知、永不建待办（即使正文里带时间、Agent 已建项也撤销） ----
        if is_promo_email(&email.subject, &email.body_text) {
            info!(
                "招聘推广邮件不建待办（重分类为 {}）: email={email_id} category={category} subject={}",
                crate::model::CATEGORY_CAREER_PROMO,
                email.subject
            );
            category = crate::model::CATEGORY_CAREER_PROMO.to_string();
            db.update_email_category(email_id, &category)?;
            if let Ok(Some(item_id)) = db.item_for_email(email_id) {
                info!("撤销推广邮件误建待办 item={item_id} email={email_id}");
                let _ = db.delete_item(item_id);
                let _ = db.clear_email_item(email_id);
            }
        }
        db.update_email_category(email_id, &category)?;
        info!("邮件分类完成 email={email_id} category={category}");

        // ---- 3. 按分类确保事项存在（Agent 可能已用 create_item 建好） ----
        let now_ts = clock.now().to_rfc3339();
        ensure_item(db, llm, cfg, email_id, &email, &category, agent_item.as_ref(), &now_ts).await?;
        // ---- 3.5 Agent 已建项但缺截止时间 → 补推算（bad case A/B/C/D 修复） ----
        if let Ok(Some(item_id)) = db.item_for_email(email_id) {
            fill_missing_deadline(db, cfg, email_id, &email, item_id, &now_ts)?;
        }
        // ---- 3.6 同公司同类型合并：以最新邮件为准（需求 4） ----
        if let Err(e) = merge_duplicate_items(db, cfg, email_id, &email) {
            warn!("合并重复事项失败（忽略）: email={email_id} err={e:#}");
        }
        if let Ok(Some(_)) = db.item_for_email(email_id) {
            report.items_created += 1;
        }
    }

    // ---- 4. 更新 UID 游标（限流/手动范围模式下不推进） ----
    if opts.limit.is_none() && opts.since.is_none() && opts.advance_cursor {
        if let Some(uid) = max_uid {
            db.kv_set(&format!("last_uid:{account_id}"), &uid.to_string())?;
        }
    }

    // ---- 5. 每次收到新邮件后触发一次检查（需求 3.2；可关闭以禁止任何发送） ----
    if opts.run_check && report.new_emails > 0 {
        info!("收到新邮件，触发一次待办检查（account={account_id}）");
        let sub = checker::run_check(db, smtp, cfg, clock).await?;
        merge_report(&mut report, sub);
    }
    info!(
        "邮件处理批次完成: account={account_id} fetched={} new={} classified={} items_created={} items_updated={} replies={} tool_rounds={} approvals={} llm_fallback_misc={} reminders_sent={} reminders_failed={} 总耗时={}ms",
        report.fetched,
        report.new_emails,
        report.classified,
        report.items_created,
        report.items_updated,
        report.replies_handled,
        report.tool_rounds,
        report.approvals_created,
        report.llm_fallback_misc,
        report.reminders_sent,
        report.reminders_failed,
        started.elapsed().as_millis()
    );
    Ok(report)
}

/// 核心待办分类：仅这些分类即使无明确截止时间也创建待办（其余分类须有截止时间）。
pub const CORE_TODO_CATEGORIES: [&str; 3] = crate::model::CORE_ACTION_CATEGORIES;

/// 招聘推广邮件检测（需求 1：宣讲会/双选会/网申推荐/投递邀请等群发只做通知）：
/// 命中推广标记，且没有“本人需要行动”的强标记时判为推广。
///
/// 强行动标记（命中即不算推广）：面试/笔试/测评邀请、面试时间、考试时间、
/// 面试/笔试/测评链接、请在 N 小时内完成、预约面试等。
fn is_promo_email(subject: &str, body: &str) -> bool {
    const PROMO_SUBJECT: [&str; 20] = [
        "宣讲会", "空中宣讲", "空宣", "双选会", "招聘会", "网申", "投递邀请", "诚邀您投递",
        "诚邀你投递", "火热进行中", "启动啦", "邀您畅聊", "智联推荐", "扫码投递", "秋招启动",
        "专场宣讲", "校园大使", "岗位详情", "招聘启动", "空宣",
    ];
    // 正文层：必须是明确的“推广活动”词（网申/校招这类高频词不算）
    const PROMO_BODY: [&str; 6] = ["宣讲会", "空中宣讲", "双选会", "招聘会", "宣讲时间", "宣讲地点"];
    const CAMPUS_CONTEXT: [&str; 7] = ["智联招聘", "智联推荐", "校园招聘", "校招", "秋招", "春招", "实习生招聘"];
    // 状态/反馈类（投递成功、感谢投递、简历已收到…）不是推广，交给反馈守卫或正常分类
    const STATUS_NOT_PROMO: [&str; 18] = [
        "投递成功", "感谢投", "已收到", "简历已收到", "申请进度", "应聘反馈", "结果通知",
        "流程通知", "录用", "offer", "Offer", "测评通知", "欢迎应聘", "应聘", "申请",
        "感谢关注", "关注并投递", "应聘登记",
    ];
    const STRONG_ACTION: [&str; 18] = [
        "面试邀请", "笔试邀请", "测评邀请", "面试通知", "笔试通知", "面试时间",
        "考试时间", "面试链接", "笔试链接", "测评链接", "预约面试", "面试登记", "在线笔试",
        "在线测评", "专业笔试", "邀请你参加面试", "诚邀你参加", "测评邀请",
    ];
    // 主题带明确推广词时优先按推广判定；否则状态/反馈类主题（投递成功/已收到/欢迎应聘…）
    // 即使正文提到宣讲会也不当作推广。
    let promo_subject = PROMO_SUBJECT.iter().any(|k| subject.contains(k));
    if !promo_subject && STATUS_NOT_PROMO.iter().any(|k| subject.contains(k)) {
        return false;
    }
    let text = format!("{subject}\n{body}");
    if STRONG_ACTION.iter().any(|k| text.contains(k)) {
        return false;
    }
    if crate::deadline::match_action_hint(&text).is_some() {
        return false; // “请在 N 小时内完成”这类明确行动要求
    }
    promo_subject
        || (PROMO_BODY.iter().any(|k| body.contains(k))
            && CAMPUS_CONTEXT.iter().any(|k| text.contains(k)))
}

/// 反馈式邮件检测（subject-only，保守启发式）：
/// 命中反馈标记（问卷/调研/结果/投递成功/感谢信等）且不含“预约/安排一次具体事务”标记时判为反馈邮件。
/// 供“含面试关键字但并非预约面试”的邮件守卫使用（需求 3）。
fn is_feedback_email(subject: &str) -> bool {
    const FEEDBACK: [&str; 26] = [
        "问卷", "调研", "反馈", "感谢信", "感谢", "谢谢", "结果通知", "面试结果",
        "笔试结果", "测评结果", "投递成功", "投递反馈", "已投递", "收到您的简历",
        "简历已收到", "简历接收", "申请进度", "进度通知", "流程通知", "筛选通过",
        "通过初筛", "笔试通过", "测评通过", "面试通过", "录用通知", "offer",
    ];
    // 只有“预约/安排一次具体事务”的强标记才推翻反馈判定（原来含“邀请/时间/链接”等宽泛词，
    // 会把“面试体验问卷邀请”误判为事务类）
    const SCHEDULE: [&str; 12] = [
        "预约", "安排面试", "面试邀请", "笔试邀请", "测评邀请", "面试通知", "笔试通知",
        "测评通知", "面试时间", "考试时间", "笔试时间", "测评时间",
    ];
    let hit = FEEDBACK.iter().any(|k| subject.contains(k));
    if !hit {
        return false;
    }
    !SCHEDULE.iter().any(|k| subject.contains(k))
}

/// 主题是否含时间信息（含时间信息的反馈类邮件按“有截止时间”规则保留建待办）。
fn subject_has_time(subject: &str) -> bool {
    if crate::deadline::match_relative_hint(subject).is_some() {
        return true;
    }
    if crate::deadline::parse_human_datetime(subject, cfg_timezone_detection()).is_some() {
        return true;
    }
    [
        "小时", "日内", "日前", "截止", "时间", "日期", "24H", "48H", "72H", "24h", "48h",
        "72h",
    ]
    .iter()
    .any(|k| subject.contains(k))
}

/// 主题时间探测用的时区（只用于判断“是否存在时间”，不参与归一化结果；任意时区均可）。
fn cfg_timezone_detection() -> chrono_tz::Tz {
    chrono_tz::Asia::Shanghai
}

/// 确保邮件有对应事项：
/// - 已存在（Agent 用 create_item 建过）→ 跳过（补填由 fill_missing_deadline 负责）；
/// - Agent 提供了结构化 item 字段 → 依其建项；
/// - 否则按分类走旧路径（todo → LLM 提取）。
///
/// 建项规则（需求 2）：通知类一律不建；待办仅限 {interview, written_test, assessment}
/// 或 能提取/推算出明确截止时间的邮件。
/// 截止时间顺序：LLM 提取 → 正文明确时间归一化 → 相对时限推算（发件时间优先、收件时间兜底）。
async fn ensure_item(
    db: &Db,
    llm: &LlmClient,
    cfg: &AppConfig,
    email_id: i64,
    email: &EmailRecord,
    category: &str,
    agent_item: Option<&serde_json::Value>,
    now: &str,
) -> Result<()> {
    if db.item_for_email(email_id)?.is_some() {
        return Ok(()); // Agent 已建项（幂等）
    }
    let cat = db.get_category(category)?;
    let is_core_action = CORE_TODO_CATEGORIES.contains(&category);
    let creates = cat
        .as_ref()
        .map(|c| c.create_item || is_core_action)
        .unwrap_or(category == crate::model::CATEGORY_TODO || is_core_action);
    if !creates {
        info!("分类 {category} 不自动建项，跳过 email={email_id}");
        return Ok(());
    }
    let kind_todo = cat
        .as_ref()
        .map(|c| c.kind == "todo")
        .unwrap_or(category == crate::model::CATEGORY_TODO || is_core_action);
    if !kind_todo {
        // 通知类不再自动创建事项（需求 2）
        info!("通知类邮件不创建事项 email={email_id} category={category}");
        return Ok(());
    }

    // 提取结构化信息：todo 类用 Agent item 字段 > LLM 提取
    let ex: TodoExtract = if let Some(v) = agent_item {
        todo_from_json(v, &email.subject)
    } else {
        match llm
            .extract_todo(&email.subject, &email.body_text, &email.from_addr)
            .await
        {
            Ok(ex) => ex,
            Err(e) => {
                warn!("待办提取失败（邮件已归档，可在界面手动重分类）: {e:#}");
                return Ok(());
            }
        }
    };

    // 截止时间推断：LLM 提取值 → 主题/正文明确时间 → 相对时限（bad case 修复）
    let sent = if email.sent_at.is_empty() {
        None
    } else {
        Some(email.sent_at.as_str())
    };
    let recv = Some(email.received_at.as_str());
    let text = format!("{} {}", email.subject, email.body_text);
    let (deadline, dnote) = crate::deadline::infer_deadline(
        &text,
        ex.deadline.clone(),
        sent,
        recv,
        cfg.timezone,
    );
    let mut notes = ex.notes.clone();
    if let Some(n) = dnote {
        notes = if notes.trim().is_empty() {
            n
        } else {
            format!("{}\n{}", notes.trim(), n)
        };
    }
    if deadline.is_some() {
        info!("截止时间推算 email={email_id} deadline={:?}", deadline);
    }

    // 严格建项规则：非核心待办分类且无截止时间 → 不建待办（需求 2）
    if !CORE_TODO_CATEGORIES.contains(&category) && deadline.is_none() {
        info!(
            "非核心待办分类且无明确截止时间，不创建事项 email={email_id} category={category} subject={}",
            email.subject
        );
        return Ok(());
    }

    // 分类校验：LLM 返回的二级类型必须是已落库的分类 id（需求 2：禁止 label 当 id）
    let item_category = ex
        .category
        .clone()
        .filter(|c| !c.trim().is_empty() && c.trim() != category)
        .filter(|c| matches!(db.get_category(c.trim()), Ok(Some(_))))
        .map(|c| c.trim().to_string())
        .unwrap_or_else(|| category.to_string());
    let mut item = Item {
        id: 0,
        kind: ItemKind::Todo,
        title: if ex.title.trim().is_empty() { email.subject.clone() } else { ex.title.clone() },
        party: ex.party.clone(),
        event: notifier::clamp_event(&ex.event),
        category: item_category,
        deadline: deadline.clone(),
        remind_at: None,
        remind_policy: String::new(),
        needs_review: deadline.is_none(),
        status: ItemStatus::Active,
        source_email_id: Some(email_id),
        account_id: email.account_id,
        notes,
        created_at: now.to_string(),
        updated_at: now.to_string(),
    };
    apply_remind_plan(cfg, &mut item, &text, now);
    let item_id = db.insert_item(&item)?;
    db.set_email_item(email_id, item_id)?;
    info!(
        "创建事项 item={item_id} category={} kind={} deadline={:?} remind_policy={}",
        item.category,
        item.kind.as_str(),
        deadline,
        item.remind_policy
    );
    Ok(())
}

/// 计算提醒策略与最早提醒时刻（分级提醒；链接失效类优先）。
fn apply_remind_plan(cfg: &AppConfig, item: &mut Item, text: &str, now: &str) {
    let link_expiry = crate::deadline::match_link_expiry_hint(text).is_some()
        && crate::deadline::match_action_hint(text).is_none();
    item.remind_policy =
        notifier::decide_remind_policy(item.deadline.as_deref(), link_expiry, now);
    item.remind_at = notifier::reminder_times(item, cfg).into_iter().next();
}

/// Agent 已建项但无截止时间 → 基于邮件正文补推算（bad case A：Agent 建项后此前跳过推算）。
fn fill_missing_deadline(
    db: &Db,
    cfg: &AppConfig,
    email_id: i64,
    email: &EmailRecord,
    item_id: i64,
    now: &str,
) -> Result<()> {
    let Some(mut it) = db.get_item(item_id)? else {
        return Ok(());
    };
    if it.kind != ItemKind::Todo || it.deadline.is_some() {
        return Ok(());
    }
    let sent = if email.sent_at.is_empty() {
        None
    } else {
        Some(email.sent_at.as_str())
    };
    let text = format!("{} {}", email.subject, email.body_text);
    let (deadline, dnote) = crate::deadline::infer_deadline(
        &text,
        None,
        sent,
        Some(email.received_at.as_str()),
        cfg.timezone,
    );
    if let Some(d) = deadline {
        it.deadline = Some(d.clone());
        let text_all = format!("{} {}", email.subject, email.body_text);
        apply_remind_plan(cfg, &mut it, &text_all, now);
        if let Some(n) = dnote {
            it.notes = if it.notes.trim().is_empty() {
                n
            } else {
                format!("{}\n{}", it.notes.trim(), n)
            };
        }
        it.needs_review = false;
        it.updated_at = now.to_string();
        db.update_item(&it)?;
        info!("Agent 建项后补填截止时间 item={item_id} email={email_id} deadline={d}");
    } else if !it.needs_review {
        // 无截止时间可推算 → 标记“待补截止时间”（Agent 建项默认 needs_review=false）
        it.needs_review = true;
        it.updated_at = now.to_string();
        db.update_item(&it)?;
        info!("Agent 建项无截止时间，标记待补 item={item_id} email={email_id}");
    }
    Ok(())
}

/// 同公司/同主题重复邮件 → 合并为一条事项，以最新邮件为准（需求 4）。
///
/// 合并策略：
/// - 保留原有事项（保留其发送历史），用最新邮件的信息覆盖标题/事件/截止时间/备注；
/// - 新事项被删除，两封邮件都指向保留的事项；
/// - 截止时间变化时清空未发送的提醒记录，由 checker 按新截止时间重建。
fn merge_duplicate_items(db: &Db, cfg: &AppConfig, email_id: i64, email: &EmailRecord) -> Result<()> {
    let Some(new_item_id) = db.item_for_email(email_id)? else {
        return Ok(());
    };
    let Some(new_item) = db.get_item(new_item_id)? else {
        return Ok(());
    };
    if new_item.kind != ItemKind::Todo {
        return Ok(());
    }

    // (a) 完全重复邮件：同主题 + 同发件人，且旧邮件已有关联事项
    if let Some(prev_email) =
        db.find_email_by_subject_from(&email.subject, &email.from_addr, email_id)?
    {
        if let Some(prev_item_id) = prev_email.item_id {
            if prev_item_id != new_item_id {
                if let Some(prev_item) = db.get_item(prev_item_id)? {
                    if prev_item.kind == ItemKind::Todo && prev_item.status == ItemStatus::Active {
                        info!(
                            "重复邮件合并: email={email_id} → 沿用事项 item={prev_item_id}（同主题同发件人）"
                        );
                        return merge_item_into(db, cfg, &prev_item, &new_item, email_id);
                    }
                }
            }
        }
    }

    // (b) 同公司 + 同分类：仅当判断为“同一件事”时合并（同一天/±24h、提醒确认邮件、改期通知），
    //     否则保留为独立事项（例如同公司不同场次的面试）。
    let party = new_item.party.trim().to_string();
    if party.is_empty() {
        return Ok(());
    }
    // 公司名做归一化比较（“卓驭”=“深圳市卓驭科技有限公司”），避免同一家公司重复建项
    let party_key = normalize_party(&party);
    let candidates: Vec<Item> = db
        .list_active_todo_items(email.account_id)?
        .into_iter()
        .filter(|c| {
            c.id != new_item_id
                && c.category == new_item.category
                && normalize_party(&c.party) == party_key
        })
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let force = ["更新", "变更", "改期", "调整", "取消", "reschedule", "update"]
        .iter()
        .any(|k| email.subject.contains(k) || email.body_text.contains(k));
    let now = chrono::DateTime::parse_from_rfc3339(&crate::db::now_str())
        .unwrap_or_else(|_| chrono::Utc::now().fixed_offset());
    if let Some(existing) = pick_merge_target(&candidates, &new_item, cfg, force, now) {
        info!(
            "同公司同类型合并: email={email_id} party={party} category={} → 更新事项 item={}（force={force}）",
            new_item.category, existing.id
        );
        return merge_item_into(db, cfg, existing, &new_item, email_id);
    }
    Ok(())
}

/// 公司名归一化：去掉常见后缀与市级前缀，用于“同一家公司”判断。
fn normalize_party(s: &str) -> String {
    let mut t = s.trim().to_lowercase().replace([' ', '\u{3000}'], "");
    for suf in [
        "集团股份有限公司",
        "股份有限公司",
        "有限责任公司",
        "科技有限公司",
        "有限公司",
        "集团",
        "公司",
        "科技",
    ] {
        if t.len() > suf.len() && t.ends_with(suf) {
            t.truncate(t.len() - suf.len());
        }
    }
    for pre in ["深圳市", "北京市", "上海市", "广州市", "杭州市", "厦门市", "南京市", "成都市"] {
        if let Some(rest) = t.strip_prefix(pre) {
            t = rest.to_string();
        }
    }
    t
}

/// 选出“同一件事”的既有事项作为合并目标：
/// - 新事项有截止时间：优先同一天 / ±24h 内的既有事项；其次既有“待补时间”的事项；改期类邮件则合并到最早的一条；
/// - 新事项无截止时间（提醒/确认邮件）：合并到最近将来到期的事项，否则合并到最新的一条。
fn pick_merge_target<'a>(
    candidates: &'a [Item],
    new: &Item,
    cfg: &AppConfig,
    force: bool,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> Option<&'a Item> {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match new.deadline.as_deref().and_then(parse) {
        Some(nd) => {
            if let Some(t) = candidates
                .iter()
                .filter(|c| {
                    c.deadline
                        .as_deref()
                        .and_then(parse)
                        .map(|cd| {
                            let same_day = cd.with_timezone(&cfg.timezone).date_naive()
                                == nd.with_timezone(&cfg.timezone).date_naive();
                            same_day || (cd - nd).num_hours().abs() <= 24
                        })
                        .unwrap_or(false)
                })
                .min_by_key(|c| c.id)
            {
                return Some(t);
            }
            if let Some(t) = candidates
                .iter()
                .filter(|c| c.deadline.is_none())
                .min_by_key(|c| c.id)
            {
                return Some(t); // 既有事项待补时间 → 用新邮件补上
            }
            if force {
                return candidates.iter().min_by_key(|c| c.id);
            }
            None
        }
        None => candidates
            .iter()
            .filter_map(|c| c.deadline.as_deref().and_then(parse).map(|d| (d, c)))
            .filter(|(d, _)| *d >= now)
            .min_by_key(|(d, _)| *d)
            .map(|(_, c)| c)
            .or_else(|| candidates.iter().max_by_key(|c| c.id)),
    }
}

/// 把 `new`（最新邮件产生的事项）合并进 `keep`（保留的事项），删除 `new`。
fn merge_item_into(db: &Db, cfg: &AppConfig, keep: &Item, new: &Item, new_email_id: i64) -> Result<()> {
    let mut merged = keep.clone();
    let deadline_changed = merged.deadline != new.deadline;
    if !new.title.trim().is_empty() {
        merged.title = new.title.clone();
    }
    if !new.event.trim().is_empty() {
        merged.event = new.event.clone();
    }
    if new.deadline.is_some() {
        merged.deadline = new.deadline.clone();
        merged.remind_policy = new.remind_policy.clone();
    }
    if !new.notes.trim().is_empty() {
        merged.notes = new.notes.clone();
    }
    merged.needs_review = merged.deadline.is_none();
    merged.source_email_id = Some(new_email_id);
    merged.updated_at = db::now_str();
    // 依据合并后的截止时间重算提醒计划
    merged.remind_at = crate::notifier::reminder_times(&merged, cfg).into_iter().next();
    db.update_item(&merged)?;
    if deadline_changed {
        let n = db.delete_unsent_send_logs(merged.id)?;
        if n > 0 {
            info!("合并后截止时间变化，清理未发送提醒 {n} 条 item={}", merged.id);
        }
    }
    db.set_email_item(new_email_id, merged.id)?;
    db.delete_item(new.id)?;
    info!(
        "事项合并完成: 保留 item={} 删除 item={} (email={new_email_id})",
        merged.id, new.id
    );
    Ok(())
}

/// 从 Agent 输出的 item JSON 提取字段（宽容解析）。
fn todo_from_json(v: &serde_json::Value, subject: &str) -> TodoExtract {
    let s = |k: &str| -> String {
        v.get(k).and_then(|x| x.as_str()).unwrap_or_default().trim().to_string()
    };
    let mut deadline = v.get("deadline").and_then(|x| x.as_str()).map(|d| d.to_string());
    if let Some(d) = &deadline {
        if chrono::DateTime::parse_from_rfc3339(d).is_err() {
            deadline = None;
        }
    }
    TodoExtract {
        party: s("party"),
        event: s("event"),
        title: if s("title").is_empty() { subject.to_string() } else { s("title") },
        deadline,
        category: None,
        notes: s("notes"),
    }
}

fn merge_report(dst: &mut RunReport, src: RunReport) {
    dst.missed_compensated += src.missed_compensated;
    dst.reminders_sent += src.reminders_sent;
    dst.reminders_failed += src.reminders_failed;
    dst.retries += src.retries;
    dst.expired += src.expired;
}

/// 检测该邮件是否为提醒回复并返回 (item_id, intent)。
async fn detect_reply(
    db: &Db,
    llm: &LlmClient,
    mail: &crate::mail::ParsedMail,
) -> Result<Option<(i64, ReplyIntent)>> {
    let mut matched: Option<i64> = None;

    // In-Reply-To / References 中的 message-id 匹配我们发送的提醒
    for mid in mail
        .in_reply_to
        .split_whitespace()
        .chain(mail.references.split_whitespace())
    {
        let mid = mid.trim_matches('<').trim_matches('>');
        if mid.is_empty() {
            continue;
        }
        if let Some(log) = db.find_send_log_by_message_id(mid)? {
            matched = Some(log.item_id);
            break;
        }
        // greenmail/客户端可能去掉尖括号；用包含匹配兜底
        let all = db.list_send_logs(None, None, 500)?;
        if let Some(log) = all.iter().find(|l| {
            l.message_id
                .as_deref()
                .map(|m| m.contains(mid) || mid.contains(m.trim_matches('<').trim_matches('>')))
                .unwrap_or(false)
        }) {
            matched = Some(log.item_id);
            break;
        }
    }

    // 主题包含提醒主题【…】→ 回复候选
    if matched.is_none() {
        let all = db.list_send_logs(None, None, 500)?;
        if let Some(log) = all.iter().find(|l| {
            !l.subject.is_empty()
                && (mail.subject.contains(&l.subject)
                    || mail
                        .subject
                        .replace("Re:", "")
                        .replace("RE:", "")
                        .trim()
                        .contains(l.subject.trim()))
        }) {
            matched = Some(log.item_id);
        }
    }

    let Some(item_id) = matched else {
        return Ok(None);
    };

    // 关键词先行（省 LLM 调用）
    let text = format!("{} {}", mail.subject, mail.body_text);
    let lower = text.to_lowercase();
    let done_hit = ["已完成", "已经完成", "已参加", "已完成请查收"]
        .iter()
        .any(|k| text.contains(k))
        || ["done", "completed", "finished"]
            .iter()
            .any(|k| lower.contains(k));
    let silent_hit = [
        "不再提醒",
        "取消提醒",
        "别发了",
        "不用提醒",
        "不需要提醒",
        "停止提醒",
    ]
    .iter()
    .any(|k| text.contains(k))
        || ["no more reminder", "stop reminding", "unsubscribe"]
            .iter()
            .any(|k| lower.contains(k));

    let intent = if done_hit {
        ReplyIntent::Done
    } else if silent_hit {
        ReplyIntent::Silent
    } else {
        // 匹配到提醒邮件但关键词未命中 → LLM 判定意图
        match llm.reply_intent(&mail.subject, &mail.body_text).await {
            Ok(i) => i,
            Err(_) => ReplyIntent::None,
        }
    };

    if intent == ReplyIntent::None {
        return Ok(None);
    }
    Ok(Some((item_id, intent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promo_emails_are_detected() {
        // 智联推荐群发：宣讲会/双选会/网申推荐/投递邀请 → 推广（不建待办）
        for subject in [
            "尊敬的孙同学【智联推荐】清原集团 2027 校招・厦门大学站",
            "尊敬的孙同学【智联推荐】苏星π成长计划—苏州银行2027届校园招聘火热进行中",
            "孙同学，你有1份算法工程师秋招双选会投递邀请，点击查看，双选会岗位详情>>",
            "【招商银行温州分行】诚邀您扫码网申投递",
            "波克 2027秋招空中宣讲会-9/16日-19:00-邀您畅聊未来！",
        ] {
            assert!(
                is_promo_email(subject, "宣讲时间：9月18日 14:30；地点：思明校区"),
                "应判为推广: {subject}"
            );
        }
    }

    #[test]
    fn real_action_emails_are_not_promo() {
        for (subject, body) in [
            ("【华为公司】诚邀您参加华为公司在线笔试，谢谢！", "考试时间 2026-09-18 09:00"),
            ("曦望sunrise-笔试邀请", "请务必在收到邮件作答通知后，72 小时内完成作答"),
            ("字节跳动校园招聘面试邀请 — 系统软件工程师", "视频面试，请提前10分钟调试设备"),
            ("国家开发银行招聘测评通知", "测评地址：http://x ，需在72小时内完成"),
            ("【Shopee】校园招聘简历更新邀请", "请于24H内完成简历信息的更新"),
        ] {
            assert!(!is_promo_email(subject, body), "不应判为推广: {subject}");
        }
    }

    #[test]
    fn party_normalization_for_merge() {
        assert_eq!(normalize_party("卓驭科技"), normalize_party("深圳市卓驭科技有限公司"));
        assert_eq!(normalize_party("京东"), normalize_party("京东集团"));
        assert_eq!(normalize_party("字节跳动"), "字节跳动");
        assert_ne!(normalize_party("京东"), normalize_party("百度"));
    }

    #[test]
    fn status_mails_are_not_promo() {
        // 投递成功/感谢投递/申请进度等状态反馈不是“招聘推广”
        for subject in [
            "【阿里巴巴校园招聘】投递成功",
            "感谢您关注并投递寒武纪",
            "【京东校招】我们已收到你的申请，请及时关注后续进展",
            "来自Shopee的应聘反馈通知",
            "国家开发银行招聘测评通知",
            "欢迎应聘DJI 大疆",
        ] {
            assert!(
                !is_promo_email(subject, "感谢投递，请关注后续校招宣讲会安排。"),
                "状态类邮件不应判为推广: {subject}"
            );
        }
    }

    #[test]
    fn feedback_guard_catches_survey_with_invitation_wording() {
        // “面试体验问卷邀请”是反馈类，不是预约面试
        assert!(is_feedback_email("快手面试体验问卷邀请"));
        assert!(is_feedback_email("字节跳动面试体验调研"));
        assert!(is_feedback_email("面试结果通知"));
        // 真正的预约/邀请不算反馈
        assert!(!is_feedback_email("字节跳动校园招聘面试邀请"));
        assert!(!is_feedback_email("【快手】在线人才测评邀请"));
    }

    #[test]
    fn promo_body_only_with_campus_context() {
        // 正文含“宣讲会”但主题无标记：仅当同时出现校招语境时判推广
        assert!(is_promo_email(
            "某公司 2027 届校园招聘",
            "我们将于9月20日举办宣讲会，欢迎参加。"
        ));
        assert!(!is_promo_email(
            "项目周会纪要",
            "会上提到下月有一场宣讲会安排。"
        ));
    }
}
