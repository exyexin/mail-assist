//! 端到端测试：本地 greenmail（SMTP 3025 / IMAP 3143）+ mock LLM + FakeClock。
//! 覆盖：收信分类 → 待办创建 → 提前 1 天精确格式提醒 → 回复静默/完成 →
//! 发送失败 30 分钟重试 → 4 个检查时刻 → 通知/对话分类 → 去重 → 无截止时间兜底 → REST API。

mod common;

use chrono::TimeZone;
use common::*;
use mail2::clock::Clock;
use mail2::model::{ItemKind, ItemStatus, SendStatus};

#[tokio::test]
async fn e2e_full_pipeline_interview_reminder_and_done_reply() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 1) 投递面试邮件
    send_mail(
        "hr@acme.com",
        &user,
        "面试邀请 - 后端开发工程师",
        "张三您好，恭喜您通过初筛，诚邀您参加字节跳动后端开发工程师岗位的技术面试。\n面试时间：2027年9月10日 14:00。请确认是否参加。",
        Some("<e2e-interview-1@acme.com>"),
        None,
    );

    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.new_emails, 1, "新邮件数");
    assert_eq!(report.items_created, 1, "创建待办数");

    // 2) 邮件入库 + 分类
    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].category, "interview");

    // 3) 待办提取正确
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.kind, ItemKind::Todo);
    assert_eq!(item.deadline.as_deref(), Some("2027-09-10T14:00:00+08:00"));
    assert_eq!(item.party, "字节跳动");
    assert_eq!(item.event, "技术面试");
    assert!(!item.needs_review);
    assert_eq!(item.remind_at.as_deref(), Some("2027-09-09T01:00:00+00:00"));

    // 4) 提前 1 天 09:00 后 → 发送提醒，主题精确匹配【类型/公司/事件/月日】
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 9, 9, 5, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_sent, 1, "提醒发送数");

    let inbox = read_inbox(&user).await;
    let reminder = inbox
        .iter()
        .find(|(s, _, _, _)| s.contains("【"))
        .expect("收件箱应有提醒邮件");
    assert_eq!(reminder.0, "【面试/字节跳动/技术面试/0910】");
    assert!(reminder.1.contains("2027-09-10"), "正文应含截止时间");
    assert!(reminder.1.contains("【已完成】") && reminder.1.contains("【不再提醒】"));

    // send_log 已记录 message-id（用于回复识别）
    let logs = app.db.list_send_logs(None, None, 10).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].status, SendStatus::Sent);
    let reminder_mid = logs[0].message_id.clone().expect("提醒应有 message-id");

    // 5) 回复【已完成】→ 事项完成，且不再发送提醒
    send_mail(
        &user,
        &user,
        "Re: 【事务预约/待办/字节跳动/技术面试/0910】",
        "已完成，谢谢提醒。",
        Some("<e2e-reply-1@acme.com>"),
        Some(&format!("<{reminder_mid}>")),
    );
    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.replies_handled, 1, "回复处理数");
    let item = app.db.get_item(item.id).unwrap().unwrap();
    assert_eq!(item.status, ItemStatus::Completed);

    // 6) 再跑检查 → 不再产生提醒
    let before = app.db.list_send_logs(None, None, 10).unwrap().len();
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 10, 12, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_sent, 0, "已完成事项不得再提醒");
    assert_eq!(app.db.list_send_logs(None, None, 10).unwrap().len(), before);

    mock.stop();
}

#[tokio::test]
async fn e2e_silent_reply_stops_reminders() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 8, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "hr@acme.com",
        &user,
        "面试邀请",
        "邀请您参加面试。面试时间：2027年9月10日 14:00。",
        Some("<e2e-interview-2@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    let item = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap()
        .remove(0);

    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 9, 9, 5, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    let reminder_mid = app
        .db
        .list_send_logs(None, None, 10)
        .unwrap()
        .remove(0)
        .message_id
        .unwrap();

    send_mail(
        &user,
        &user,
        "Re: 提醒",
        "不再提醒，谢谢。",
        Some("<e2e-reply-2@acme.com>"),
        Some(&format!("<{reminder_mid}>")),
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    let item = app.db.get_item(item.id).unwrap().unwrap();
    assert_eq!(item.status, ItemStatus::Silent);

    let before = app.db.list_send_logs(None, None, 10).unwrap().len();
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 10, 12, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_sent, 0);
    assert_eq!(app.db.list_send_logs(None, None, 10).unwrap().len(), before);

    mock.stop();
}

#[tokio::test]
async fn e2e_failed_send_retries_after_30_minutes() {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(
        &mock.base_url,
        &user,
        1, /* 坏端口：注入 SMTP 故障 */
    );
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 直接建一个带截止时间的事项
    let now = mail2::db::now_str();
    let item = mail2::model::Item {
        id: 0,
        kind: ItemKind::Todo,
        title: "故障重试测试".into(),
        party: "测试公司".into(),
        event: "故障重试测试".into(),
        category: "todo".into(),
        deadline: Some("2027-09-10T14:00:00+08:00".into()),
        remind_at: Some("2027-09-09T01:00:00+00:00".into()),
        remind_policy: String::new(),
        needs_review: false,
        status: ItemStatus::Active,
        source_email_id: None,
        account_id: 1,
        notes: String::new(),
        created_at: now.clone(),
        updated_at: now,
    };
    let item_id = app.db.insert_item(&item).unwrap();

    // 提醒时刻已到 → 发送失败 → next_retry = now + 30min
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 9, 9, 5, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_failed, 1);

    let log = app.db.list_send_logs(None, None, 10).unwrap().remove(0);
    assert_eq!(log.status, SendStatus::Failed);
    assert_eq!(log.attempt, 1);
    let retry = log.next_retry_at.clone().unwrap();
    let retry_dt = chrono::DateTime::parse_from_rfc3339(&retry).unwrap();
    let now_dt =
        chrono::DateTime::parse_from_rfc3339(&clock.now().with_timezone(&chrono::Utc).to_rfc3339())
            .unwrap();
    // 约 30 分钟后（容差 5 分钟，覆盖执行耗时）
    let delta = (retry_dt - now_dt).num_minutes();
    assert!(
        (25..=35).contains(&delta),
        "重试应延迟约 30 分钟，实际 {delta} 分钟"
    );

    // 立刻再查 → 不到重试时间，不再发送（attempt 不变）
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_failed, 0, "未到 30 分钟不得重试");
    let log = app.db.list_send_logs(None, None, 10).unwrap().remove(0);
    assert_eq!(log.attempt, 1);

    // 30 分钟后仍失败 → attempt 递增
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 9, 9, 36, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_failed, 1);
    let log = app.db.list_send_logs(None, None, 10).unwrap().remove(0);
    assert_eq!(log.attempt, 2);

    // 再等 30 分钟，换成正常 SMTP → 重试成功，提醒邮件送达
    clock.advance(chrono::Duration::minutes(31));
    let good_app = build_app(&dir, clock.clone(), Some(GREENMAIL_SMTP.1));
    let report = mail2::checker::run_check(
        &good_app.db,
        &good_app.smtp,
        &good_app.cfg,
        good_app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.reminders_sent, 1);
    let log = app.db.list_send_logs(None, None, 10).unwrap().remove(0);
    assert_eq!(log.status, SendStatus::Sent);
    let inbox = read_inbox(&user).await;
    assert!(inbox
        .iter()
        .any(|(s, _, _, _)| s == "【事务预约/待办/测试公司/故障重试测试/0910】"));

    // 事项仍在 active（发送成功不改变状态）
    let item = app.db.get_item(item_id).unwrap().unwrap();
    assert_eq!(item.status, ItemStatus::Active);

    mock.stop();
}

#[tokio::test]
async fn e2e_check_times_fire_once_per_day() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 5, 59);
    let app = build_app(&dir, clock.clone(), None);

    // 每个检查时刻触发一次；同一天重复 tick 不重复触发
    for (h, m) in [(6, 0), (12, 30), (18, 30), (0, 0)] {
        clock.set(
            chrono_tz::Asia::Shanghai
                .with_ymd_and_hms(2027, 9, 1, h, m, 0)
                .unwrap()
                .with_timezone(&chrono::Local),
        );
        mail2::scheduler::tick(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
            .await
            .unwrap();
        let key = format!("check_fired:2027-09-01 {:02}:{:02}", h, m);
        let v = app.db.kv_get(&key).unwrap();
        assert!(v.is_some(), "检查时刻 {h:02}:{m:02} 应触发");
    }
    // 同一天同一时刻再次 tick → 不覆盖记录（去重）
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 1, 6, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let before = app
        .db
        .kv_get("check_fired:2027-09-01 06:00")
        .unwrap()
        .unwrap();
    mail2::scheduler::tick(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    let after = app
        .db
        .kv_get("check_fired:2027-09-01 06:00")
        .unwrap()
        .unwrap();
    assert_eq!(before, after, "同一天同一时刻只应触发一次");

    // 非检查时刻 → 不触发
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 1, 7, 7, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    mail2::scheduler::tick(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert!(app
        .db
        .kv_get("check_fired:2027-09-01 07:07")
        .unwrap()
        .is_none());

    mock.stop();
}

#[tokio::test]
async fn e2e_notification_conversation_misc_and_dedup() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 通知类 → 建通知事务
    send_mail(
        "billing@bank.com",
        &user,
        "8月账单",
        "您的账单已出，请查收。",
        Some("<n-1@b.com>"),
        None,
    );
    // 对话类 → 不建事务
    send_mail(
        "friend@x.com",
        &user,
        "讨论一下方案",
        "关于方案的讨论，你怎么看？",
        Some("<c-1@x.com>"),
        None,
    );
    // 其他 → 不建事务
    send_mail(
        "spam@x.com",
        &user,
        "广告推广",
        "限时优惠，速来抢购。",
        Some("<m-1@x.com>"),
        None,
    );

    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.new_emails, 3);
    assert_eq!(report.items_created, 0, "通知/对话/其他类均不应建事项（需求：通知类不再建项）");

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 0, "通知/对话/其他邮件不得创建任何事项");

    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    let cats: Vec<&str> = emails.iter().map(|e| e.category.as_str()).collect();
    assert!(
        cats.contains(&"notification") && cats.contains(&"conversation") && cats.contains(&"misc")
    );

    // 去重：重复投递相同 message-id → 不再入库
    send_mail(
        "billing@bank.com",
        &user,
        "8月账单",
        "您的账单已出，请查收。",
        Some("<n-1@b.com>"),
        None,
    );
    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.new_emails, 0, "重复邮件应被去重");
    assert_eq!(app.db.list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() }).unwrap().len(), 3);

    mock.stop();
}

#[tokio::test]
async fn e2e_missing_deadline_marks_needs_review() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "hr@acme.com",
        &user,
        "面试邀约",
        "邀请您参加面试，具体时间后续通知，暂时没有时间安排。",
        Some("<e2e-interview-3@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].needs_review, "无截止时间应标记待补充");
    assert!(items[0].deadline.is_none());
    assert!(items[0].remind_at.is_none());

    // 无截止时间 → 检查器不产生提醒
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 12, 9, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.reminders_sent, 0);
    assert_eq!(report.missed_compensated, 0);

    mock.stop();
}

#[tokio::test]
async fn api_crud_and_actions() {
    use axum::http::StatusCode;

    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);
    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        dir.path()
            .join("web-nonexist")
            .to_string_lossy()
            .to_string(),
    );
    let router = mail2::api::router(state);

    // health
    let resp = call(&router, "GET", "/api/v1/health", String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["code"], 0);
    assert_eq!(v["data"]["status"], "ok");

    // 创建待办
    let resp = call(
        &router,
        "POST",
        "/api/v1/items",
        r#"{"kind":"todo","title":"API测试","party":"测试公司","event":"API测试","deadline":"2027-09-10T14:00:00+08:00","notes":"n"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    let item_id = v["data"]["id"].as_i64().unwrap();
    assert_eq!(v["data"]["status"], "active");

    // 列表
    let resp = call(&router, "GET", "/api/v1/items?kind=todo", String::new()).await;
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["data"].as_array().unwrap().len(), 1);

    // 更新（改截止时间）
    let resp = call(
        &router,
        "PUT",
        &format!("/api/v1/items/{item_id}"),
        r#"{"kind":"todo","title":"API测试2","party":"测试公司","event":"API测试","deadline":"2027-09-12T14:00:00+08:00"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["data"]["title"], "API测试2");

    // 非法 deadline → 400
    let resp = call(
        &router,
        "PUT",
        &format!("/api/v1/items/{item_id}"),
        r#"{"kind":"todo","title":"x","deadline":"not-a-date"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 完成
    let resp = call(
        &router,
        "POST",
        &format!("/api/v1/items/{item_id}/complete"),
        String::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["data"]["status"], "completed");

    // 立即提醒（非 active）→ 400
    let resp = call(
        &router,
        "POST",
        &format!("/api/v1/items/{item_id}/remind-now"),
        String::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 删除
    let resp = call(
        &router,
        "DELETE",
        &format!("/api/v1/items/{item_id}"),
        String::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(
        &router,
        "GET",
        &format!("/api/v1/items/{item_id}"),
        String::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 检查器端点
    let resp = call(&router, "POST", "/api/v1/checker/run", String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 配置端点（脱敏）
    let resp = call(&router, "GET", "/api/v1/config", String::new()).await;
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["data"]["llm"]["api_key"], "***");
    assert_eq!(v["data"]["mail"]["imap"]["password"], "***");

    // 404 语义
    let resp = call(&router, "GET", "/api/v1/emails/999999", String::new()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    mock.stop();
}

/// 对 Router 发起一次内存请求（tower oneshot）。
async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: String,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let mut b = Request::builder().method(method).uri(uri);
    if !body.is_empty() {
        b = b.header("content-type", "application/json");
    }
    router
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

/// 从响应提取 JSON body。
async fn resp_json(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap()
}

// ================= 新需求 e2e =================

/// 需求 9.1：Agent 改操作 → 审批单 → 用户批准后重放生效；拒绝则不生效。
#[tokio::test]
async fn e2e_agent_mutate_requires_approval_and_replay() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 预置一个事项（id=1）
    let now = mail2::db::now_str();
    let item = mail2::model::Item {
        id: 0,
        kind: ItemKind::Todo,
        title: "原始标题".into(),
        party: "某公司".into(),
        event: "某事项".into(),
        category: "todo".into(),
        deadline: Some("2027-09-10T14:00:00+08:00".into()),
        remind_at: None,
        remind_policy: String::new(),
        needs_review: false,
        status: ItemStatus::Active,
        source_email_id: None,
        account_id: 1,
        notes: String::new(),
        created_at: now.clone(),
        updated_at: now,
    };
    let item_id = app.db.insert_item(&item).unwrap();

    // 邮件带 REQ_UPDATE_ITEM 标记 → mock agent 调用 update_item 工具
    send_mail(
        "hr@acme.com",
        &user,
        "请修改事项标题",
        "REQ_UPDATE_ITEM 请把事项标题改一下。",
        Some("<e2e-mutate-1@acme.com>"),
        None,
    );
    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.new_emails, 1);
    assert_eq!(report.approvals_created, 1, "改操作应生成审批申请");

    // 数据未被改动（pending）
    assert_eq!(app.db.get_item(item_id).unwrap().unwrap().title, "原始标题");
    let approvals = app.db.list_approvals(Some("pending"), 10).unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].tool_name, "update_item");

    // agent_runs 审计存在
    let runs = app.db.list_agent_runs(10).unwrap();
    assert!(
        runs.iter().any(|r| r.email_id.is_some() && r.tool_calls >= 1 && r.status == "ok"),
        "应有 agent 运行审计记录"
    );

    // API 批准 → 重放执行
    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);
    let aid = approvals[0].id;
    let resp = call(&router, "POST", &format!("/api/v1/approvals/{aid}/approve"), String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["approved"], true);
    assert_eq!(
        app.db.get_item(item_id).unwrap().unwrap().title,
        "Agent 修改的标题",
        "批准后应重放执行"
    );

    // 再触发一次改操作 → 拒绝 → 不生效
    send_mail(
        "hr@acme.com",
        &user,
        "再次请求修改",
        "REQ_UPDATE_ITEM 请再次修改标题。",
        Some("<e2e-mutate-2@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    let pending = app.db.list_approvals(Some("pending"), 10).unwrap();
    let aid2 = pending[0].id;
    let resp = call(&router, "POST", &format!("/api/v1/approvals/{aid2}/reject"), String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        app.db.get_item(item_id).unwrap().unwrap().title,
        "Agent 修改的标题",
        "拒绝后不得生效"
    );

    mock.stop();
}

/// 需求 1：手动范围拉取只处理范围内的邮件，且不推进 UID 游标（后续轮询补收）。
#[tokio::test]
async fn e2e_manual_range_fetch_and_no_cursor_advance() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    let d = |h: u32| -> chrono::DateTime<chrono::Utc> {
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 1, h, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    // A：3 天内（10:00）；B：超出 3 天（发件时间 8 月 20 日）
    send_mail_at("hr@acme.com", &user, "范围内的面试", "面试邀请，面试时间：2027年9月10日 14:00。", Some("<e2e-range-a@acme.com>"), None, Some(d(10)));
    send_mail_at("hr@acme.com", &user, "范围外的会议", "会议通知。", Some("<e2e-range-b@acme.com>"), None, Some(d(10) - chrono::Duration::days(12)));

    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);
    // 近 3 天手动拉取
    let resp = call(&router, "POST", "/api/v1/mail/fetch", r#"{"range":"3d"}"#.into()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["fetched"], 2, "全量拉取到 2 封");
    assert_eq!(v["data"]["new_emails"], 1, "只有范围内 1 封入库");

    let emails = app.db.list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].subject, "范围内的面试");

    // 游标未推进 → 常规轮询补收范围外邮件
    assert!(app.db.kv_get("last_uid:1").unwrap().is_none(), "手动拉取不得推进游标");
    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(report.new_emails, 1, "轮询应补收范围外邮件");
    let emails = app.db.list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(emails.len(), 2);
    assert!(app.db.kv_get("last_uid:1").unwrap().is_some(), "轮询后推进游标");

    mock.stop();
}

/// 需求 6：过期清扫 → expired；不再提醒；可恢复（恢复后改期则保持 active）。
#[tokio::test]
async fn e2e_expired_sweep_and_activate() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "hr@acme.com",
        &user,
        "面试邀请",
        "邀请您参加面试。面试时间：2027年9月10日 14:00。",
        Some("<e2e-expire-1@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    let item = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap()
        .remove(0);
    assert_eq!(item.status, ItemStatus::Active);

    // 越过截止时间 → 检查器清扫为 expired，且不补发提醒
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 11, 12, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.expired, 1, "应标记 1 个过期");
    assert_eq!(report.reminders_sent, 0, "过期事项不得补发提醒");
    assert_eq!(
        app.db.get_item(item.id).unwrap().unwrap().status,
        ItemStatus::Expired
    );

    // API 恢复 → active；改期到未来 → 不再被清扫
    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);
    let resp = call(&router, "POST", &format!("/api/v1/items/{}/activate", item.id), String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(app.db.get_item(item.id).unwrap().unwrap().status, ItemStatus::Active);

    let resp = call(
        &router,
        "PUT",
        &format!("/api/v1/items/{}", item.id),
        r#"{"kind":"todo","title":"改期面试","party":"某公司","event":"技术面试","deadline":"2027-09-20T14:00:00+08:00"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let report = mail2::checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
        .await
        .unwrap();
    assert_eq!(report.expired, 0, "改期后不再过期");
    assert_eq!(app.db.get_item(item.id).unwrap().unwrap().status, ItemStatus::Active);

    mock.stop();
}

/// 需求 7 + 8：记录发件时间；无明确截止时间 + "请在48小时内完成" → 发件时间 + 48h。
#[tokio::test]
async fn e2e_relative_deadline_from_sent_at() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    let sent = chrono_tz::Asia::Shanghai
        .with_ymd_and_hms(2027, 9, 1, 10, 0, 0)
        .unwrap()
        .with_timezone(&chrono::Utc);
    send_mail_at(
        "hr@acme.com",
        &user,
        "笔试邀请",
        "恭喜进入笔试环节，请在48小时内完成作答。",
        Some("<e2e-relative-1@acme.com>"),
        None,
        Some(sent),
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    // 需求 8：发件时间入库（UTC 归一化，比较时刻而非字符串偏移）
    let emails = app.db.list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(emails.len(), 1);
    let expect_sent = chrono::DateTime::parse_from_rfc3339("2027-09-01T10:00:00+08:00").unwrap();
    let actual_sent = chrono::DateTime::parse_from_rfc3339(&emails[0].sent_at).unwrap();
    assert_eq!(actual_sent, expect_sent, "应记录发件时间");

    // 需求 7：推算截止时间 = 发件时间 + 48h
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1);
    let actual_ddl =
        chrono::DateTime::parse_from_rfc3339(items[0].deadline.as_deref().expect("应有截止时间")).unwrap();
    assert_eq!(
        actual_ddl,
        expect_sent + chrono::Duration::hours(48),
        "截止时间应为发件时间 + 48 小时"
    );
    assert!(!items[0].needs_review, "已推算截止时间无需人工补充");
    assert!(items[0].notes.contains("推算"), "备注应说明推算依据");

    mock.stop();
}

/// 需求 3：账户 CRUD + 连接测试；需求 2/4：邮件列表按发件时间倒序 + 批量重分类/删除（含时间区间）。
#[tokio::test]
async fn e2e_accounts_sorting_and_batch() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);

    // ---- 账户 CRUD ----
    let resp = call(&router, "GET", "/api/v1/accounts", String::new()).await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "种子默认账户");
    assert_eq!(v["data"][0]["is_default"], true);
    assert_eq!(v["data"][0]["imap_password"], "***", "密码脱敏");

    let resp = call(
        &router,
        "POST",
        "/api/v1/accounts",
        format!(r#"{{"label":"第二邮箱","address":"{user}","imap_host":"127.0.0.1","imap_port":3143,"imap_user":"{user}","imap_password":"x","smtp_host":"127.0.0.1","smtp_port":3025,"smtp_password":"x"}}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = resp_json(resp).await;
    let acct2 = v["data"]["id"].as_i64().unwrap();

    // 测试连接：正确端口成功
    let resp = call(&router, "POST", &format!("/api/v1/accounts/{acct2}/test"), String::new()).await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["ok"], true, "greenmail IMAP 应可连接: {}", v["data"]["message"]);

    // 测试连接：错误端口失败
    let resp = call(
        &router,
        "POST",
        &format!("/api/v1/accounts/{acct2}/test"),
        r#"{"imap_port":3144}"#.into(),
    )
    .await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["ok"], false);

    // 更新 + 设默认 + 删除
    let resp = call(
        &router,
        "PUT",
        &format!("/api/v1/accounts/{acct2}"),
        r#"{"label":"改名邮箱","imap_password":"***","smtp_password":"***"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(&router, "POST", &format!("/api/v1/accounts/{acct2}/default"), String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(&router, "GET", "/api/v1/accounts", String::new()).await;
    let v = resp_json(resp).await;
    let a2 = v["data"].as_array().unwrap().iter().find(|a| a["id"] == acct2).unwrap();
    assert_eq!(a2["label"], "改名邮箱");
    assert_eq!(a2["is_default"], true);

    let resp = call(&router, "DELETE", &format!("/api/v1/accounts/{acct2}"), String::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // ---- 邮件倒序（按收件时间 received_at）与批量 ----
    let d = |day: u32| -> chrono::DateTime<chrono::Utc> {
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, day, 10, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    // 分批发送 + 推进时钟，使各封邮件收件时间可区分（收件时间 = 处理时的时钟时间）
    for (day, subj) in [(1, "账单-1日"), (2, "账单-2日"), (3, "账单-3日")] {
        clock.set(
            chrono_tz::Asia::Shanghai
                .with_ymd_and_hms(2027, 9, day, 10, 0, 0)
                .unwrap()
                .with_timezone(&chrono::Local),
        );
        send_mail_at(
            "billing@bank.com",
            &user,
            subj,
            "您的账单已出，请查收。",
            Some(&format!("<e2e-sort-{day}@b.com>")),
            None,
            Some(d(day)),
        );
        mail2::pipeline::process_new_mails(
            &app.db,
            &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
            &app.smtp,
            &app.llm,
            &app.cfg,
            app.clock.as_ref(),
        )
        .await
        .unwrap();
    }
    let emails = app.db.list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(emails.len(), 3);
    let subjects: Vec<&str> = emails.iter().map(|e| e.subject.as_str()).collect();
    assert_eq!(subjects, vec!["账单-3日", "账单-2日", "账单-1日"], "默认按收件时间倒序");

    // 批量重分类（勾选 ids）
    let ids: Vec<i64> = emails.iter().take(2).map(|e| e.id).collect();
    let resp = call(
        &router,
        "POST",
        "/api/v1/emails/batch",
        format!(r#"{{"ids":[{},{}],"action":"reclassify","category":"misc"}}"#, ids[0], ids[1]),
    )
    .await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["changed"], 2);

    // 批量删除（按时间区间：9月2日及之前发件的）
    let resp = call(
        &router,
        "POST",
        "/api/v1/emails/batch",
        r#"{"filters":{"before":"2027-09-02T00:00:00+00:00"},"action":"delete"}"#.into(),
    )
    .await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["changed"], 1);
    assert_eq!(
        app.db.list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() }).unwrap().len(),
        2
    );

    // 批量参数校验：ids/filters 都为空 → 400
    let resp = call(&router, "POST", "/api/v1/emails/batch", r#"{"action":"delete"}"#.into()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // ---- 事项批量（时间区间） ----
    let resp = call(
        &router,
        "POST",
        "/api/v1/items",
        r#"{"kind":"todo","title":"批量事项A","party":"某公司","event":"事项A","deadline":"2027-09-20T14:00:00+08:00"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(
        &router,
        "POST",
        "/api/v1/items/batch",
        r#"{"filters":{"created_after":"2020-01-01T00:00:00+00:00"},"action":"complete"}"#.into(),
    )
    .await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["changed"].as_i64().unwrap() >= 1, true);
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert!(items.iter().all(|i| i.status == ItemStatus::Completed));

    mock.stop();
}

/// 需求 9.1（增/查自主）：Agent 读邮件 → create_category 建分类 → 落库。
#[tokio::test]
async fn e2e_agent_create_category_autonomous() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 通过 API 手工新增分类（模拟用户/LLM 增分类路径）
    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);
    let resp = call(
        &router,
        "POST",
        "/api/v1/categories",
        r#"{"id":"online_assessment","label":"线上测评","create_item":true,"kind":"todo"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["created"], true);
    let resp = call(&router, "GET", "/api/v1/categories", String::new()).await;
    let v = resp_json(resp).await;
    assert!(v["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "online_assessment" && c["label"] == "线上测评"));

    // 非法 id → 400
    let resp = call(
        &router,
        "POST",
        "/api/v1/categories",
        r#"{"id":"BAD ID!","label":"坏分类"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    mock.stop();
}

// ================= 需求 2/3/4/5 新增用例 =================

/// bad case A（需求 4）：Agent 直接 create_item 建项（deadline=null）后，
/// 流水线必须依据正文“24H内完成”补推算截止时间（此前 Agent 建项路径会跳过推算）。
#[tokio::test]
async fn e2e_agent_created_item_gets_relative_deadline_filled() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    let sent = chrono_tz::Asia::Shanghai
        .with_ymd_and_hms(2027, 9, 1, 10, 0, 0)
        .unwrap()
        .with_timezone(&chrono::Utc);
    send_mail_at(
        "hr@acme.com",
        &user,
        "REQ_CREATE_ITEM 笔试邀请",
        "恭喜进入笔试环节，请在24H内完成作答。",
        Some("<e2e-agent-24h@acme.com>"),
        None,
        Some(sent),
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1, "Agent 建项应保留");
    let it = &items[0];
    assert_eq!(it.kind, ItemKind::Todo);
    let expect =
        chrono::DateTime::parse_from_rfc3339("2027-09-01T10:00:00+08:00").unwrap()
            + chrono::Duration::hours(24);
    let actual =
        chrono::DateTime::parse_from_rfc3339(it.deadline.as_deref().expect("应补填截止时间")).unwrap();
    assert_eq!(actual, expect, "Agent 建项后应依据正文“24H内完成”补推算截止时间");
    assert!(!it.needs_review, "已补截止时间无需人工补充");
    assert!(it.notes.contains("推算"), "备注应说明推算依据: {}", it.notes);
    assert!(it.remind_at.is_some(), "应生成提醒时刻");

    mock.stop();
}

/// 需求 3：含“面试”关键字但为反馈/结果通知类邮件 → 不建待办，统一归为 notification。
#[tokio::test]
async fn e2e_feedback_email_with_interview_keyword_not_todo() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // mock 默认把“面试”归为 interview（模拟 Agent 误判），流水线守卫应纠正
    send_mail(
        "hr@kuaishou.com",
        &user,
        "面试结果通知",
        "您申请的岗位面试结果已出，请登录招聘官网查看。",
        Some("<e2e-feedback-1@kuaishou.com>"),
        None,
    );
    // 面试问卷类
    send_mail(
        "hr@bytedance.com",
        &user,
        "【面试体验】面试问卷",
        "感谢您参加面试，诚邀您填写面试体验问卷（约2分钟）。",
        Some("<e2e-feedback-2@bytedance.com>"),
        None,
    );

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 2);
    for e in &emails {
        assert_eq!(
            e.category, "notification",
            "反馈式邮件应归为 notification: {}",
            e.subject
        );
    }
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 0, "反馈式邮件不得创建待办");

    mock.stop();
}

/// bad case B（需求 4）：正文中人类可读明确时间 “2026-04-24 11:00(GMT+08:00)”
/// 应被归一化为 RFC3339 截止时间（此前会被丢弃）。
#[tokio::test]
async fn e2e_explicit_human_time_normalized_to_deadline() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2026, 4, 20, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    let sent = chrono_tz::Asia::Shanghai
        .with_ymd_and_hms(2026, 4, 20, 10, 0, 0)
        .unwrap()
        .with_timezone(&chrono::Utc);
    send_mail_at(
        "hr@kuaishou.com",
        &user,
        "面试邀请",
        "面试时间：2026-04-24 11:00(GMT+08:00)\n面试链接：https://viewcoder.example.com/x\n请在规定时间参加。",
        Some("<e2e-human-time-1@kuaishou.com>"),
        None,
        Some(sent),
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1, "应创建待办");
    assert_eq!(
        items[0].deadline.as_deref(),
        Some("2026-04-24T11:00:00+08:00"),
        "人类可读时间应归一化为 RFC3339"
    );
    assert!(!items[0].needs_review);
    assert!(items[0].notes.contains("推算"), "备注应说明来源: {}", items[0].notes);

    mock.stop();
}

/// 需求 2（严格规则）：非核心待办分类（如事务预约类）无明确截止时间 → 不创建待办。
#[tokio::test]
async fn e2e_generic_todo_without_deadline_not_created() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "hr@acme.com",
        &user,
        "会议通知",
        "邀请您参加项目评审会议，具体时间后续通知，暂时没有时间安排。",
        Some("<e2e-todo-noddl@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 0, "无截止时间的非核心分类邮件不建待办（需求：待办仅含面试/笔试/测评与有截止时间的邮件）");
    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 1, "邮件仍应入库归档");
    assert_eq!(emails[0].category, "todo");

    mock.stop();
}

/// 需求 2：重分类为通知类不再自动创建通知事项（仅改分类）。
#[tokio::test]
async fn e2e_reclassify_to_notification_no_item() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "hr@acme.com",
        &user,
        "笔试邀请",
        "请在48小时内完成作答。",
        Some("<e2e-reclass-1@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 1);
    let email_id = emails[0].id;
    let items_before = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items_before.len(), 1, "笔试邀请应创建待办");

    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);
    let resp = call(
        &router,
        "POST",
        &format!("/api/v1/emails/{email_id}/reclassify"),
        r#"{"category":"notification"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["item_updated"], false, "重分类为通知不应新建事项");

    let items_after = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(
        items_after.len(),
        0,
        "重分类为通知类应撤销此前误建的待办（通知类不保留事项）"
    );

    mock.stop();
}

/// 需求 5：邮件标注 API（保存 → 列表返回与过滤 → 清除 → 404）。
#[tokio::test]
async fn e2e_email_label_api() {
    use axum::http::StatusCode;
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "spam@x.com",
        &user,
        "广告推广",
        "限时优惠，速来抢购。",
        Some("<lbl-1@x.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 1);
    let email_id = emails[0].id;

    let state = mail2::api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        String::new(),
    );
    let router = mail2::api::router(state);

    // 保存标注
    let resp = call(
        &router,
        "PUT",
        &format!("/api/v1/emails/{email_id}/label"),
        r#"{"label":"deadline_missing","note":"应在24H内完成但未记录截止时间"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = resp_json(resp).await;
    assert_eq!(v["data"]["label"], "deadline_missing");

    // 列表返回标注字段 + 按标注过滤
    let resp = call(&router, "GET", "/api/v1/emails?label=deadline_missing", String::new()).await;
    let v = resp_json(resp).await;
    let arr = v["data"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["user_label"], "deadline_missing");
    assert_eq!(arr[0]["user_note"], "应在24H内完成但未记录截止时间");

    // 未标注过滤应排除它
    let resp = call(&router, "GET", "/api/v1/emails?label=__none__", String::new()).await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 0);

    // 清除标注
    let resp = call(
        &router,
        "PUT",
        &format!("/api/v1/emails/{email_id}/label"),
        r#"{"label":"","note":""}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = call(&router, "GET", "/api/v1/emails", String::new()).await;
    let v = resp_json(resp).await;
    assert_eq!(v["data"][0]["user_label"], "");

    // 不存在 → 404
    let resp = call(
        &router,
        "PUT",
        "/api/v1/emails/9999/label",
        r#"{"label":"x"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    mock.stop();
}

/// 待办/通知事项按关联邮件收件时间（received_at）倒序；手动创建（无邮件）事项按创建时间兜底。
#[tokio::test]
async fn e2e_items_sorted_by_email_received_time() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 第一批：笔试邀请 → received_at = 9/1 10:00 → written_test 事项
    send_mail(
        "hr@acme.com",
        &user,
        "笔试邀请A",
        "恭喜进入笔试环节，请在24H内完成作答。",
        Some("<e2e-order-1@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    // 第二批：面试邀请 → received_at = 9/3 10:00（晚于笔试）→ interview 事项
    clock.set(
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2027, 9, 3, 10, 0, 0)
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    send_mail(
        "hr@acme.com",
        &user,
        "面试邀请B",
        "面试时间：2027年9月10日 14:00。请确认是否参加。",
        Some("<e2e-order-2@acme.com>"),
        None,
    );
    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    // 手动建一个无关联邮件的事项（创建时间为真实 now，早于两条 2027 邮件 → 排最后）
    let manual = app
        .db
        .insert_item(&mail2::model::Item {
            id: 0,
            kind: ItemKind::Todo,
            title: "手动事项".into(),
            party: "手动".into(),
            event: "手动事项".into(),
            category: "todo".into(),
            deadline: None,
            remind_at: None,
            remind_policy: String::new(),
            needs_review: false,
            status: ItemStatus::Active,
            source_email_id: None,
            account_id: 1,
            notes: String::new(),
            created_at: mail2::db::now_str(),
            updated_at: mail2::db::now_str(),
        })
        .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    // 面试事项（received 9/3）> 笔试事项（received 9/1）> 手动事项（created 兜底）
    let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["技术面试邀请", "线上笔试邀请", "手动事项"],
        "事项按关联邮件收件时间倒序，手动事项回退创建时间"
    );
    assert!(items[0].source_email_id.is_some());
    assert!(items[1].source_email_id.is_some());
    assert_eq!(items[2].id, manual);

    mock.stop();
}

/// 需求 1（待办口径）：宣讲会/双选会/网申推荐/投递邀请等招聘推广邮件只做通知、不建待办。
#[tokio::test]
async fn e2e_promo_email_no_item() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // 正文含“会议”会让 mock 归为 todo 并给出 deadline（模拟 Agent 误判为事务）
    send_mail(
        "campus@mailf.wisdomore.com",
        &user,
        "尊敬的孙同学【智联推荐】清原集团 2027 校招・厦门大学站",
        "宣讲会时间：2027-09-20 19:10；地点：厦门大学翔安校区；欢迎参加项目评审会。",
        Some("<e2e-promo-1@wisdomore.com>"),
        None,
    );
    send_mail(
        "campus@mailf.wisdomore.com",
        &user,
        "孙同学，你有1份算法工程师秋招双选会投递邀请，双选会岗位详情>>",
        "双选会报名时间：2027年8月1日至9月30日，点击查看岗位详情。",
        Some("<e2e-promo-2@wisdomore.com>"),
        None,
    );

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 2);
    for e in &emails {
        assert_eq!(
            e.category, "career_promo",
            "招聘推广邮件应归为 career_promo: {}",
            e.subject
        );
    }
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 0, "招聘推广邮件不得创建待办");

    mock.stop();
}

/// 需求 1：即使 Agent 已经用 create_item 建了待办，推广守卫也要撤销。
#[tokio::test]
async fn e2e_promo_guard_revokes_agent_item() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    // REQ_CREATE_ITEM 使 mock Agent 调用 create_item（deadline=null）
    send_mail(
        "campus@mailf.wisdomore.com",
        &user,
        "【智联推荐】某银行2027届秋季校园招聘宣讲会火热进行中",
        "宣讲会时间：2027-09-20 19:10。REQ_CREATE_ITEM",
        Some("<e2e-promo-revoke@wisdomore.com>"),
        None,
    );

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 0, "Agent 误建的推广待办应被撤销");

    mock.stop();
}

/// 需求 4：完全重复的邮件（同主题同发件人）只保留一条事项，两封邮件都指向它。
#[tokio::test]
async fn e2e_duplicate_emails_merge_into_one_item() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    for mid in ["<e2e-dup-1@nowcoder.net>", "<e2e-dup-2@nowcoder.net>"] {
        send_mail(
            "support@batchmail.nowcoder.net",
            &user,
            "京东集团邀请你参加在线笔试",
            "考试时间 2027-09-05 19:00-21:00（北京时间），请提前调试设备。",
            Some(mid),
            None,
        );
    }

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 2);
    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1, "重复邮件只应保留一条待办");
    let ids: Vec<i64> = emails.iter().filter_map(|e| e.item_id).collect();
    assert_eq!(ids.len(), 2, "两封邮件都应关联到事项");
    assert_eq!(ids[0], ids[1], "两封重复邮件应指向同一事项");

    mock.stop();
}

/// 需求 4：同公司同类型（面试）的多封邮件合并为一条，以最新邮件为准。
#[tokio::test]
async fn e2e_same_company_same_type_merged() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail_at(
        "nio.talent@mail.feishu.cn",
        &user,
        "【NIO蔚来】邀请你预约校招面试时间",
        "请点击链接自助预约视频面试时间。",
        Some("<e2e-merge-1@nio.com>"),
        None,
        Some(
            chrono::DateTime::parse_from_rfc3339("2027-09-01T01:00:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    );
    send_mail_at(
        "nio.talent@mail.feishu.cn",
        &user,
        "【NIO蔚来】诚邀你参加校招-大模型推理框架工程师面试",
        "面试时间：2027-09-10 14:00（北京时间）。请提前10分钟调试设备。",
        Some("<e2e-merge-2@nio.com>"),
        None,
        Some(
            chrono::DateTime::parse_from_rfc3339("2027-09-02T01:00:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    );

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(
        items.len(),
        1,
        "同公司同类型（面试）应合并为一条待办: {:?}",
        items
            .iter()
            .map(|i| format!("{}|{}|{}|{:?}", i.party, i.category, i.event, i.deadline))
            .collect::<Vec<_>>()
    );
    assert_eq!(items[0].party, "字节跳动"); // mock LLM 固定返回该 party
    assert!(
        items[0].source_email_id.is_some(),
        "合并后应指向最新来源邮件"
    );

    mock.stop();
}

/// 需求 3（分级提醒）：24h 内紧急 → 收到即提醒 + 截止前 2h（两条提醒计划）。
#[tokio::test]
async fn e2e_urgent_short_deadline_graded_reminders() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail_at(
        "hr@shopee.com",
        &user,
        "曦望sunrise-笔试邀请",
        "请务必在收到邮件作答通知后，24 小时内完成作答。",
        Some("<e2e-urgent-1@nowcoder.net>"),
        None,
        Some(
            chrono::DateTime::parse_from_rfc3339("2027-09-01T02:00:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    );

    let report = mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1, "含 24H 内完成的邮件应建待办");
    let item = &items[0];
    assert_eq!(
        item.remind_policy,
        mail2::model::remind_policy::URGENT,
        "24h 内截止应为紧急提醒策略"
    );
    // 截止 = 发件 2027-09-01T02:00Z + 24h = 2027-09-02T02:00Z
    assert_eq!(item.deadline.as_deref(), Some("2027-09-02T02:00:00+00:00"));
    // 立即提醒应已在本次收信后的检查中发出
    assert_eq!(report.reminders_sent, 1, "收到即提醒应已发送");
    let logs = app.db.list_send_logs(Some(item.id), None, 10).unwrap();
    assert_eq!(logs.len(), 1, "此刻只应发出“收到即提醒”一条");
    assert_eq!(logs[0].status, SendStatus::Sent);

    // 推进到截止前 2 小时 → 第二次（临期）提醒发出
    clock.set(
        chrono::DateTime::parse_from_rfc3339("2027-09-02T00:00:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Local),
    );
    let r2 = mail2::checker::run_check(
        &app.db,
        &app.smtp,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(r2.reminders_sent, 1, "截止前 2h 应发出第二次提醒");
    let logs = app.db.list_send_logs(Some(item.id), None, 10).unwrap();
    assert_eq!(logs.len(), 2, "紧急事项应有两次提醒（立即 + 截止前 2h）");

    mock.stop();
}

/// 需求 3：链接失效类（“链接将于 24 小时后失效”）→ 识别为截止时间并立即提醒。
#[tokio::test]
async fn e2e_link_expiry_immediate_reminder() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail_at(
        "nio.talent@mail.feishu.cn",
        &user,
        "【NIO蔚来】邀请你预约校招面试时间",
        "请在收到本邮件后尽快自助选择面试时间（链接将于 24 小时后失效，请尽快操作）。",
        Some("<e2e-link-expiry@nio.com>"),
        None,
        Some(
            chrono::DateTime::parse_from_rfc3339("2027-09-01T02:00:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    );

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(
        item.deadline.as_deref(),
        Some("2027-09-02T02:00:00+00:00"),
        "24 小时后失效应推算为截止时间"
    );
    assert_eq!(
        item.remind_policy,
        mail2::model::remind_policy::LINK_EXPIRY,
        "链接失效类应为立即提醒策略"
    );
    assert_eq!(item.needs_review, false, "失效类不应再标记待补截止时间");

    mock.stop();
}

/// 需求 4：同公司但不同场次（截止时间相差 >24h）的面试保留为独立待办；
/// 同一场次的“面试提醒”邮件则合并进既有待办。
#[tokio::test]
async fn e2e_same_company_different_events_not_merged() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    for (mid, subject, dl) in [
        ("<e2e-2events-1@bytedance.com>", "字节跳动面试邀约（第一场）", "2027-09-10T14:00:00+08:00"),
        ("<e2e-2events-2@bytedance.com>", "字节跳动面试邀约（第二场）", "2027-09-20T14:00:00+08:00"),
        // 同一场次的提醒邮件（同一天）→ 应合并进第二场
        ("<e2e-2events-3@bytedance.com>", "字节跳动面试提醒", "2027-09-20T14:00:00+08:00"),
    ] {
        send_mail(
            "people@mail.bytedance.net",
            &user,
            subject,
            &format!("面试时间见正文。DEADLINE:{dl}"),
            Some(mid),
            None,
        );
    }

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    let items = app
        .db
        .list_items(&mail2::db::ItemFilter { limit: 10, ..Default::default() })
        .unwrap();
    let mut dls: Vec<String> = items
        .iter()
        .filter_map(|i| i.deadline.clone())
        .collect();
    dls.sort();
    assert_eq!(
        dls,
        vec![
            "2027-09-10T14:00:00+08:00".to_string(),
            "2027-09-20T14:00:00+08:00".to_string()
        ],
        "不同场次面试应保留为两条待办，同场次提醒应合并"
    );

    mock.stop();
}

/// 回归：自动拉取必须用 BODY.PEEK[]，不得把用户邮箱里的邮件标记为已读（\Seen）。
#[tokio::test]
async fn e2e_fetch_does_not_mark_mail_as_seen() {
    ensure_greenmail();
    let user = unique_user();
    let mock = MockLlmServer::start().await;
    let dir = temp_config(&mock.base_url, &user, GREENMAIL_SMTP.1);
    let clock = fake_clock(2027, 9, 1, 10, 0);
    let app = build_app(&dir, clock.clone(), None);

    send_mail(
        "hr@example.com",
        &user,
        "字节跳动校园招聘面试邀请",
        "面试时间：2027-09-10 14:00（北京时间）。",
        Some("<e2e-unseen-1@bytedance.com>"),
        None,
    );
    assert_eq!(common::unseen_count(&user).await, 1, "投递后应为未读");

    mail2::pipeline::process_new_mails(
        &app.db,
        &mail2::mail::ImapClient::from_config(&app.cfg.mail.imap),
        &app.smtp,
        &app.llm,
        &app.cfg,
        app.clock.as_ref(),
    )
    .await
    .unwrap();

    // 拉取并分类后，邮箱中该邮件仍应保持未读
    assert_eq!(
        common::unseen_count(&user).await,
        1,
        "自动拉取不得把邮件标记为已读（应使用 BODY.PEEK[]）"
    );
    let emails = app
        .db
        .list_emails(&mail2::db::EmailFilter { limit: 10, ..Default::default() })
        .unwrap();
    assert_eq!(emails.len(), 1, "邮件内容仍应正常入库");

    mock.stop();
}
