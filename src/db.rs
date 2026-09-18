//! SQLite 持久化（WAL + busy_timeout；Mutex 单连接，临界区为毫秒级短查询）。
//!
//! 迁移策略：基础表用 CREATE TABLE IF NOT EXISTS（含新列）；
//! 旧库补列通过 PRAGMA table_info 检查后 ALTER TABLE；PRAGMA user_version 记录版本。

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::AppConfig;
use crate::model::{
    AgentRun, Approval, CategoryInfo, EmailRecord, Item, ItemKind, ItemStatus, MailAccount,
    SendLog, SendStatus,
};

/// 数据库句柄：**邮件域**与**待办域**分属两个独立 SQLite 文件。
///
/// - `mail` 邮件库（`mail.db`）：emails / accounts / agent_runs / kv（收信游标、调度标记）
/// - `todo` 待办库（`todo.db`）：items / send_log / approvals / categories
///
/// 两库之间只保留整数引用（`emails.item_id` ↔ `items.source_email_id`），不做外键约束；
/// 跨域排序（待办按关联邮件收件时间）在应用层完成，因此不需要跨库 JOIN。
/// 同一个 Mutex 保护两个连接，避免并发锁顺序问题。
pub struct Db {
    conns: Mutex<Conns>,
}

/// 两个数据库文件的连接集合。
pub struct Conns {
    pub mail: Connection,
    pub todo: Connection,
}

/// 邮件库表结构（新建 + 旧库补列）。
fn mail_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS emails (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            uid INTEGER,
            message_id TEXT NOT NULL UNIQUE,
            subject TEXT NOT NULL DEFAULT '',
            from_addr TEXT NOT NULL DEFAULT '',
            from_name TEXT NOT NULL DEFAULT '',
            body_text TEXT NOT NULL DEFAULT '',
            category TEXT NOT NULL DEFAULT '',
            sent_at TEXT NOT NULL DEFAULT '',
            received_at TEXT NOT NULL DEFAULT '',
            item_id INTEGER,
            reply_to_item_id INTEGER,
            account_id INTEGER NOT NULL DEFAULT 1,
            handled INTEGER NOT NULL DEFAULT 0,
            user_label TEXT NOT NULL DEFAULT '',
            user_note TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS accounts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            label TEXT NOT NULL DEFAULT '',
            address TEXT NOT NULL DEFAULT '',
            imap_host TEXT NOT NULL DEFAULT '',
            imap_port INTEGER NOT NULL DEFAULT 993,
            imap_user TEXT NOT NULL DEFAULT '',
            imap_password TEXT NOT NULL DEFAULT '',
            imap_tls_insecure INTEGER NOT NULL DEFAULT 0,
            smtp_host TEXT NOT NULL DEFAULT '',
            smtp_port INTEGER NOT NULL DEFAULT 465,
            smtp_user TEXT NOT NULL DEFAULT '',
            smtp_password TEXT NOT NULL DEFAULT '',
            smtp_tls_insecure INTEGER NOT NULL DEFAULT 0,
            enabled INTEGER NOT NULL DEFAULT 1,
            is_default INTEGER NOT NULL DEFAULT 0,
            reminder_to TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS agent_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            email_id INTEGER,
            account_id INTEGER NOT NULL DEFAULT 1,
            rounds INTEGER NOT NULL DEFAULT 0,
            tool_calls INTEGER NOT NULL DEFAULT 0,
            final_json TEXT NOT NULL DEFAULT '',
            trace TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'ok',
            created_at TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_emails_category ON emails(category);
        "#,
    )
    .context("邮件库建表失败")?;
    // 旧库补列（存在即跳过）
    if !column_exists(conn, "emails", "sent_at")? {
        conn.execute_batch("ALTER TABLE emails ADD COLUMN sent_at TEXT NOT NULL DEFAULT '';")
            .context("emails.sent_at 迁移失败")?;
    }
    if !column_exists(conn, "emails", "account_id")? {
        conn.execute_batch("ALTER TABLE emails ADD COLUMN account_id INTEGER NOT NULL DEFAULT 1;")
            .context("emails.account_id 迁移失败")?;
    }
    if !column_exists(conn, "emails", "user_label")? {
        conn.execute_batch("ALTER TABLE emails ADD COLUMN user_label TEXT NOT NULL DEFAULT '';")
            .context("emails.user_label 迁移失败")?;
    }
    if !column_exists(conn, "emails", "user_note")? {
        conn.execute_batch("ALTER TABLE emails ADD COLUMN user_note TEXT NOT NULL DEFAULT '';")
            .context("emails.user_note 迁移失败")?;
    }
    Ok(())
}

/// 待办库表结构（新建 + 旧库补列 + 分类规则迁移）。
fn todo_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS items (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL DEFAULT 'todo',
            title TEXT NOT NULL DEFAULT '',
            party TEXT NOT NULL DEFAULT '',
            event TEXT NOT NULL DEFAULT '',
            category TEXT NOT NULL DEFAULT '',
            deadline TEXT,
            remind_at TEXT,
            remind_policy TEXT NOT NULL DEFAULT 'normal',
            needs_review INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'active',
            source_email_id INTEGER,
            account_id INTEGER NOT NULL DEFAULT 1,
            notes TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS send_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            item_id INTEGER NOT NULL,
            attempt INTEGER NOT NULL DEFAULT 0,
            kind TEXT NOT NULL DEFAULT 'reminder',
            to_addr TEXT NOT NULL DEFAULT '',
            subject TEXT NOT NULL DEFAULT '',
            body TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'pending',
            error TEXT,
            scheduled_at TEXT NOT NULL DEFAULT '',
            sent_at TEXT,
            next_retry_at TEXT,
            message_id TEXT
        );
        CREATE TABLE IF NOT EXISTS categories (
            id TEXT PRIMARY KEY,
            label TEXT NOT NULL DEFAULT '',
            create_item INTEGER NOT NULL DEFAULT 1,
            kind TEXT NOT NULL DEFAULT 'todo',
            source TEXT NOT NULL DEFAULT 'builtin',
            created_at TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS approvals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tool_name TEXT NOT NULL DEFAULT '',
            payload_json TEXT NOT NULL DEFAULT '',
            summary TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'pending',
            source_email_id INTEGER,
            decided_at TEXT,
            created_at TEXT NOT NULL DEFAULT ''
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sendlog_item_sched ON send_log(item_id, scheduled_at);
        CREATE INDEX IF NOT EXISTS idx_sendlog_status ON send_log(status);
        CREATE INDEX IF NOT EXISTS idx_items_status ON items(status);
        CREATE INDEX IF NOT EXISTS idx_items_deadline ON items(deadline);
        "#,
    )
    .context("待办库建表失败")?;
    if !column_exists(conn, "items", "account_id")? {
        conn.execute_batch("ALTER TABLE items ADD COLUMN account_id INTEGER NOT NULL DEFAULT 1;")
            .context("items.account_id 迁移失败")?;
    }
    if !column_exists(conn, "items", "remind_policy")? {
        conn.execute_batch(
            "ALTER TABLE items ADD COLUMN remind_policy TEXT NOT NULL DEFAULT 'normal';",
        )
        .context("items.remind_policy 迁移失败")?;
    }
    // 通知类邮件不再自动创建事项
    conn.execute(
        "UPDATE categories SET create_item=0, kind='notification' WHERE id='notification'",
        [],
    )
    .context("通知分类建项规则迁移失败")?;
    // 招聘推广分类（宣讲会/双选会/网申推荐/投递邀请）：开箱即用，默认不建项
    conn.execute(
        "INSERT OR IGNORE INTO categories (id,label,create_item,kind,source,created_at)
         VALUES ('career_promo','招聘推广',0,'notification','builtin',?1)",
        params![now_str()],
    )
    .context("招聘推广分类迁移失败")?;
    // 历史遗留：曾由 Agent 自建、默认建项的推广类分类（如 career_talk）统一改为不建项
    conn.execute(
        "UPDATE categories SET create_item=0, kind='notification'
         WHERE source='llm' AND (id IN ('career_talk','宣讲会','campus_talk') OR label LIKE '%宣讲%' OR label LIKE '%推广%')",
        [],
    )
    .context("推广分类建项规则迁移失败")?;
    conn.pragma_update(None, "user_version", 5)
        .context("设置 user_version 失败")?;
    Ok(())
}

/// 首次启动时把旧的单库 `mail2.db` 拆分到 `mail.db` + `todo.db`（旧文件改名 `.bak` 保留）。
///
/// 仅当旧文件存在、且两个新文件都还不存在时执行；数据按表归属分别复制（显式列名，容忍旧结构缺列）。
fn split_legacy_database(legacy: &Path, mail_path: &Path, todo_path: &Path) -> Result<bool> {
    if !legacy.exists() || mail_path.exists() || todo_path.exists() {
        return Ok(false);
    }
    let conn = Connection::open(legacy)
        .with_context(|| format!("打开旧库 {} 失败", legacy.display()))?;
    // 旧库先补齐两域的表与列，保证复制时列齐全
    mail_schema(&conn)?;
    todo_schema(&conn)?;
    // 先建立两个新库的表结构（独立连接创建，随后用 ATTACH 复制数据）
    {
        let m = Connection::open(mail_path)
            .with_context(|| format!("创建邮件库 {} 失败", mail_path.display()))?;
        mail_schema(&m)?;
        let t = Connection::open(todo_path)
            .with_context(|| format!("创建待办库 {} 失败", todo_path.display()))?;
        todo_schema(&t)?;
    }
    conn.execute("ATTACH DATABASE ?1 AS newmail", params![mail_path.to_string_lossy()])
        .context("附加新邮件库失败")?;
    conn.execute("ATTACH DATABASE ?1 AS newtodo", params![todo_path.to_string_lossy()])
        .context("附加新待办库失败")?;
    let copy_sql = r#"
        INSERT OR IGNORE INTO newmail.emails
            (id,uid,message_id,subject,from_addr,from_name,body_text,category,sent_at,received_at,
             item_id,reply_to_item_id,account_id,handled,user_label,user_note)
        SELECT id,uid,message_id,subject,from_addr,from_name,body_text,category,sent_at,received_at,
             item_id,reply_to_item_id,account_id,handled,user_label,user_note FROM main.emails;
        INSERT OR IGNORE INTO newmail.accounts
            (id,label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
             smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,
             reminder_to,created_at,updated_at)
        SELECT id,label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
             smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,
             reminder_to,created_at,updated_at FROM main.accounts;
        INSERT OR IGNORE INTO newmail.agent_runs
            (id,email_id,account_id,rounds,tool_calls,final_json,trace,status,created_at)
        SELECT id,email_id,account_id,rounds,tool_calls,final_json,trace,status,created_at FROM main.agent_runs;
        INSERT OR IGNORE INTO newmail.kv (k,v) SELECT k,v FROM main.kv;

        INSERT OR IGNORE INTO newtodo.items
            (id,kind,title,party,event,category,deadline,remind_at,remind_policy,needs_review,status,
             source_email_id,account_id,notes,created_at,updated_at)
        SELECT id,kind,title,party,event,category,deadline,remind_at,remind_policy,needs_review,status,
             source_email_id,account_id,notes,created_at,updated_at FROM main.items;
        INSERT OR IGNORE INTO newtodo.send_log
            (id,item_id,attempt,kind,to_addr,subject,body,status,error,scheduled_at,sent_at,next_retry_at,message_id)
        SELECT id,item_id,attempt,kind,to_addr,subject,body,status,error,scheduled_at,sent_at,next_retry_at,message_id FROM main.send_log;
        INSERT OR IGNORE INTO newtodo.categories (id,label,create_item,kind,source,created_at)
        SELECT id,label,create_item,kind,source,created_at FROM main.categories;
        INSERT OR IGNORE INTO newtodo.approvals
            (id,tool_name,payload_json,summary,status,source_email_id,decided_at,created_at)
        SELECT id,tool_name,payload_json,summary,status,source_email_id,decided_at,created_at FROM main.approvals;
    "#;
    conn.execute_batch(copy_sql).context("拆分旧库数据失败")?;
    let emails: i64 = conn
        .query_row("SELECT COUNT(*) FROM newmail.emails", [], |r| r.get(0))
        .context("统计新邮件库失败")?;
    let items: i64 = conn
        .query_row("SELECT COUNT(*) FROM newtodo.items", [], |r| r.get(0))
        .context("统计新待办库失败")?;
    conn.execute_batch("DETACH DATABASE newmail; DETACH DATABASE newtodo;")
        .context("分离新库失败")?;
    drop(conn);
    // 旧文件改名保留（含 WAL/SHM 附属文件）
    let bak = legacy.with_extension("db.bak");
    std::fs::rename(legacy, &bak)
        .with_context(|| format!("重命名旧库为 {} 失败", bak.display()))?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", legacy.display()));
        if side.exists() {
            let _ = std::fs::rename(&side, PathBuf::from(format!("{}{suffix}", bak.display())));
        }
    }
    tracing::info!(
        "已拆分旧库 {} → {}（emails={emails}）+ {}（items={items}），旧文件保留为 {}",
        legacy.display(),
        mail_path.display(),
        todo_path.display(),
        bak.display()
    );
    Ok(true)
}

impl Db {
    /// 打开（必要时创建）邮件库与待办库；`path` 为 `None` 时使用 data_dir 下的默认文件名。
    pub fn open(data_dir: PathBuf, mail_db: Option<PathBuf>, todo_db: Option<PathBuf>) -> Result<Db> {
        std::fs::create_dir_all(&data_dir).with_context(|| "创建数据目录失败")?;
        let mail_path = mail_db.unwrap_or_else(|| data_dir.join("mail.db"));
        let todo_path = todo_db.unwrap_or_else(|| data_dir.join("todo.db"));
        for p in [&mail_path, &todo_path] {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建数据库目录 {} 失败", parent.display()))?;
            }
        }
        // 旧单库自动拆分（一次性）
        split_legacy_database(&data_dir.join("mail2.db"), &mail_path, &todo_path)?;

        let mail = Connection::open(&mail_path)
            .with_context(|| format!("打开邮件库 {}", mail_path.display()))?;
        let todo = Connection::open(&todo_path)
            .with_context(|| format!("打开待办库 {}", todo_path.display()))?;
        for c in [&mail, &todo] {
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;",
            )
            .context("初始化 PRAGMA 失败")?;
        }
        let db = Db {
            conns: Mutex::new(Conns { mail, todo }),
        };
        db.migrate()?;
        Ok(db)
    }

    fn conns(&self) -> std::sync::MutexGuard<'_, Conns> {
        self.conns.lock().expect("db mutex poisoned")
    }

    fn migrate(&self) -> Result<()> {
        let c = self.conns();
        mail_schema(&c.mail)?;
        todo_schema(&c.todo)?;
        Ok(())
    }

    /// 首次启动种子：config 静态邮箱 → 默认账户（邮件库）；llm.yaml 分类 → categories（待办库）。
    pub fn seed_defaults(&self, cfg: &AppConfig) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .context("统计账户失败")?;
        if n == 0 {
            let a = MailAccount::from_config(&cfg.mail);
            conn.execute(
                "INSERT INTO accounts (label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
                        smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,reminder_to,created_at,updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1,1,?13,?14,?14)",
                params![
                    a.label,
                    a.address,
                    a.imap_host,
                    a.imap_port,
                    a.imap_user,
                    MailAccount::obfuscate(&a.imap_password),
                    a.imap_tls_insecure as i64,
                    a.smtp_host,
                    a.smtp_port,
                    a.smtp_user,
                    MailAccount::obfuscate(&a.smtp_password),
                    a.smtp_tls_insecure as i64,
                    a.reminder_to,
                    a.created_at,
                ],
            )
            .context("种子默认账户失败")?;
        }
        // 分类：始终补齐 llm.yaml 中的种子分类（INSERT OR IGNORE，幂等）。
        // 注意：不能再用“表为空才种子”的判断——迁移会预先插入 career_promo，
        // 会导致整套种子分类缺失（Agent 只能自建分类，分类质量崩坏）。
        let conn = &c.todo;
        for c in &cfg.llm.categories {
            conn.execute(
                "INSERT OR IGNORE INTO categories (id,label,create_item,kind,source,created_at)
                 VALUES (?1,?2,?3,?4,'builtin',?5)",
                params![
                    c.id,
                    c.label,
                    c.create_item as i64,
                    default_kind(&c.id, c.create_item),
                    now_str(),
                ],
            )
            .context("种子分类失败")?;
        }
        Ok(())
    }

    // ---------- kv ----------
    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        let c = self.conns();
        let conn = &c.mail;
        let v: Option<String> = conn
            .query_row("SELECT v FROM kv WHERE k=?1", params![k], |r| r.get(0))
            .optional()
            .context("kv_get 失败")?;
        Ok(v)
    }

    pub fn kv_set(&self, k: &str, v: &str) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![k, v],
        )
        .context("kv_set 失败")?;
        Ok(())
    }

    // ---------- emails ----------
    /// 插入邮件；message_id 已存在则返回 Ok(None)（去重）。
    pub fn insert_email(&self, e: &EmailRecord) -> Result<Option<i64>> {
        let c = self.conns();
        let conn = &c.mail;
        let n = conn
            .execute(
                "INSERT OR IGNORE INTO emails
                 (uid, message_id, subject, from_addr, from_name, body_text, category, sent_at, received_at, reply_to_item_id, account_id, handled)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    e.uid,
                    e.message_id,
                    e.subject,
                    e.from_addr,
                    e.from_name,
                    e.body_text,
                    e.category,
                    e.sent_at,
                    e.received_at,
                    e.reply_to_item_id,
                    e.account_id,
                    e.handled as i64,
                ],
            )
            .context("insert_email 失败")?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(conn.last_insert_rowid()))
    }

    pub fn update_email_category(&self, id: i64, category: &str) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "UPDATE emails SET category=?1 WHERE id=?2",
            params![category, id],
        )
        .context("update_email_category 失败")?;
        Ok(())
    }

    pub fn set_email_item(&self, id: i64, item_id: i64) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "UPDATE emails SET item_id=?1 WHERE id=?2",
            params![item_id, id],
        )
        .context("set_email_item 失败")?;
        Ok(())
    }

    /// 保存/清除用户标注（label 为空 = 清除标注）。
    pub fn update_email_label(&self, id: i64, label: &str, note: &str) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "UPDATE emails SET user_label=?1, user_note=?2 WHERE id=?3",
            params![label, note, id],
        )
        .context("update_email_label 失败")?;
        Ok(())
    }

    /// 解除邮件与事项的关联（撤销误建待办时使用）。
    pub fn clear_email_item(&self, id: i64) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute("UPDATE emails SET item_id=NULL WHERE id=?1", params![id])
            .context("clear_email_item 失败")?;
        Ok(())
    }

    /// 删除某事项尚未发送的提醒记录（截止时间变更后重建，避免发出过期内容）。
    pub fn delete_unsent_send_logs(&self, item_id: i64) -> Result<usize> {
        let c = self.conns();
        let conn = &c.todo;
        let n = conn
            .execute(
                "DELETE FROM send_log WHERE item_id=?1 AND status<>'sent'",
                params![item_id],
            )
            .context("delete_unsent_send_logs 失败")?;
        Ok(n)
    }

    /// 邮件对应的事项 id（agent 幂等检查用）
    pub fn item_for_email(&self, email_id: i64) -> Result<Option<i64>> {
        let c = self.conns();
        let conn = &c.mail;
        let v: Option<Option<i64>> = conn
            .query_row(
                "SELECT item_id FROM emails WHERE id=?1",
                params![email_id],
                |r| r.get(0),
            )
            .optional()
            .context("item_for_email 失败")?;
        Ok(v.flatten())
    }

    pub fn delete_email(&self, id: i64) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute("DELETE FROM emails WHERE id=?1", params![id])
            .context("delete_email 失败")?;
        Ok(())
    }

    pub fn get_email(&self, id: i64) -> Result<Option<EmailRecord>> {
        let c = self.conns();
        let conn = &c.mail;
        let e = conn
            .query_row(
                "SELECT id, uid, message_id, subject, from_addr, from_name, body_text, category,
                        sent_at, received_at, item_id, reply_to_item_id, account_id, handled,
                        user_label, user_note
                 FROM emails WHERE id=?1",
                params![id],
                row_to_email,
            )
            .optional()
            .context("get_email 失败")?;
        Ok(e)
    }

    pub fn list_emails(&self, f: &EmailFilter) -> Result<Vec<EmailRecord>> {
        let c = self.conns();
        let conn = &c.mail;
        let mut sql = String::from(
            "SELECT id, uid, message_id, subject, from_addr, from_name, body_text, category,
                    sent_at, received_at, item_id, reply_to_item_id, account_id, handled,
                    user_label, user_note
             FROM emails WHERE 1=1",
        );
        let mut ps: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(c) = f.category.as_deref() {
            if !c.is_empty() {
                sql.push_str(" AND category=?");
                ps.push(Box::new(c.to_string()));
            }
        }
        if let Some(a) = f.account_id {
            sql.push_str(" AND account_id=?");
            ps.push(Box::new(a));
        }
        if let Some(l) = f.label.as_deref() {
            if !l.is_empty() {
                if l == crate::model::email_label::NONE || l == "__none__" {
                    sql.push_str(" AND user_label=''");
                } else {
                    sql.push_str(" AND user_label=?");
                    ps.push(Box::new(l.to_string()));
                }
            }
        }
        if let Some(q) = f.q.as_deref() {
            let q = q.trim();
            if !q.is_empty() {
                sql.push_str(" AND (subject LIKE ? OR from_addr LIKE ? OR from_name LIKE ? OR body_text LIKE ?)");
                let like = format!("%{q}%");
                ps.push(Box::new(like.clone()));
                ps.push(Box::new(like.clone()));
                ps.push(Box::new(like.clone()));
                ps.push(Box::new(like));
            }
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        ps.push(Box::new(f.limit as i64));
        let mut stmt = conn.prepare(&sql).context("list_emails 失败")?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ps.iter().map(|p| p.as_ref())), row_to_email)
            .context("list_emails 查询失败")?;
        let mut out: Vec<EmailRecord> = rows.collect::<rusqlite::Result<_>>()?;
        // 时间区间过滤（收件时间优先，发件时间兜底）：本地比较避免跨时区字符串比较误差
        if let Some(since) = f.since.as_deref() {
            out.retain(|e| {
                let key = if e.received_at.is_empty() { &e.sent_at } else { &e.received_at };
                cmp_time(key, since).is_ge()
            });
        }
        if let Some(before) = f.before.as_deref() {
            out.retain(|e| {
                let key = if e.received_at.is_empty() { &e.sent_at } else { &e.received_at };
                cmp_time(key, before).is_le()
            });
        }
        // 默认按收件时间倒序（无收件时间回退发件时间）
        out.sort_by(|a, b| {
            let ka = if a.received_at.is_empty() { &a.sent_at } else { &a.received_at };
            let kb = if b.received_at.is_empty() { &b.sent_at } else { &b.received_at };
            cmp_time(kb, ka).then(b.id.cmp(&a.id))
        });
        Ok(out)
    }

    pub fn batch_update_email_category(&self, ids: &[i64], category: &str) -> Result<usize> {
        let c = self.conns();
        let conn = &c.mail;
        let mut n = 0;
        for id in ids {
            n += conn
                .execute(
                    "UPDATE emails SET category=?1 WHERE id=?2",
                    params![category, id],
                )
                .context("批量重分类失败")?;
        }
        Ok(n)
    }

    pub fn batch_delete_emails(&self, ids: &[i64]) -> Result<usize> {
        let c = self.conns();
        let conn = &c.mail;
        let mut n = 0;
        for id in ids {
            n += conn
                .execute("DELETE FROM emails WHERE id=?1", params![id])
                .context("批量删除邮件失败")?;
        }
        Ok(n)
    }

    // ---------- items ----------
    pub fn insert_item(&self, it: &Item) -> Result<i64> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "INSERT INTO items (kind,title,party,event,category,deadline,remind_at,remind_policy,needs_review,status,source_email_id,account_id,notes,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                it.kind.as_str(),
                it.title,
                it.party,
                it.event,
                it.category,
                it.deadline,
                it.remind_at,
                it.remind_policy,
                it.needs_review as i64,
                it.status.as_str(),
                it.source_email_id,
                it.account_id,
                it.notes,
                it.created_at,
                it.updated_at,
            ],
        )
        .context("insert_item 失败")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_item(&self, it: &Item) -> Result<()> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "UPDATE items SET kind=?1,title=?2,party=?3,event=?4,category=?5,deadline=?6,remind_at=?7,
                    remind_policy=?8,needs_review=?9,status=?10,notes=?11,updated_at=?12 WHERE id=?13",
            params![
                it.kind.as_str(),
                it.title,
                it.party,
                it.event,
                it.category,
                it.deadline,
                it.remind_at,
                it.remind_policy,
                it.needs_review as i64,
                it.status.as_str(),
                it.notes,
                it.updated_at,
                it.id,
            ],
        )
        .context("update_item 失败")?;
        Ok(())
    }

    /// 列出某账户的全部 active 待办（合并同公司同类型事项时用于候选筛选，规模很小）。
    pub fn list_active_todo_items(&self, account_id: i64) -> Result<Vec<Item>> {
        let c = self.conns();
        let conn = &c.todo;
        let mut stmt = conn
            .prepare(
                "SELECT id,kind,title,party,event,category,deadline,remind_at,remind_policy,needs_review,status,
                        source_email_id,account_id,notes,created_at,updated_at
                 FROM items
                 WHERE account_id=?1 AND status='active' AND kind='todo'
                 ORDER BY id ASC",
            )
            .context("list_active_todo_items 失败")?;
        let rows = stmt
            .query_map(params![account_id], row_to_item)
            .context("list_active_todo_items 查询失败")?;
        let out: Vec<Item> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(out)
    }

    /// 查找同主题 + 同发件人的历史邮件（用于重复邮件识别）。
    pub fn find_email_by_subject_from(
        &self,
        subject: &str,
        from_addr: &str,
        exclude_id: i64,
    ) -> Result<Option<EmailRecord>> {
        let c = self.conns();
        let conn = &c.mail;
        let e = conn
            .query_row(
                "SELECT id,uid,message_id,subject,from_addr,from_name,body_text,category,sent_at,received_at,
                        item_id,reply_to_item_id,account_id,handled,user_label,user_note
                 FROM emails WHERE subject=?1 AND from_addr=?2 AND id<>?3
                 ORDER BY id DESC LIMIT 1",
                params![subject, from_addr, exclude_id],
                row_to_email,
            )
            .optional()
            .context("find_email_by_subject_from 失败")?;
        Ok(e)
    }

    pub fn delete_item(&self, id: i64) -> Result<()> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute("DELETE FROM items WHERE id=?1", params![id])
            .context("delete_item 失败")?;
        conn.execute("DELETE FROM send_log WHERE item_id=?1", params![id])
            .context("delete_item 日志清理失败")?;
        Ok(())
    }

    pub fn set_item_status(&self, id: i64, status: ItemStatus) -> Result<()> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "UPDATE items SET status=?1, updated_at=?2 WHERE id=?3",
            params![status.as_str(), crate::db::now_str(), id],
        )
        .context("set_item_status 失败")?;
        Ok(())
    }

    pub fn get_item(&self, id: i64) -> Result<Option<Item>> {
        let c = self.conns();
        let conn = &c.todo;
        let it = conn
            .query_row(
                "SELECT id,kind,title,party,event,category,deadline,remind_at,remind_policy,needs_review,status,
                        source_email_id,account_id,notes,created_at,updated_at
                 FROM items WHERE id=?1",
                params![id],
                row_to_item,
            )
            .optional()
            .context("get_item 失败")?;
        Ok(it)
    }

    pub fn list_items(&self, f: &ItemFilter) -> Result<Vec<Item>> {
        let c = self.conns();
        let conn = &c.todo;
        let mut sql = String::from(
            "SELECT id,kind,title,party,event,category,deadline,remind_at,remind_policy,needs_review,status,
                    source_email_id,account_id,notes,created_at,updated_at
             FROM items WHERE 1=1",
        );
        let mut ps: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(k) = f.kind {
            sql.push_str(" AND kind=?");
            ps.push(Box::new(k.as_str().to_string()));
        }
        if let Some(s) = f.status {
            sql.push_str(" AND status=?");
            ps.push(Box::new(s.as_str().to_string()));
        }
        if let Some(c) = f.category.as_deref() {
            if !c.is_empty() {
                sql.push_str(" AND category=?");
                ps.push(Box::new(c.to_string()));
            }
        }
        if let Some(p) = f.party.as_deref() {
            if !p.is_empty() {
                sql.push_str(" AND party=?");
                ps.push(Box::new(p.to_string()));
            }
        }
        if let Some(a) = f.account_id {
            sql.push_str(" AND account_id=?");
            ps.push(Box::new(a));
        }
        if let Some(q) = f.q.as_deref() {
            let q = q.trim();
            if !q.is_empty() {
                sql.push_str(" AND (title LIKE ? OR party LIKE ? OR event LIKE ? OR notes LIKE ?)");
                let like = format!("%{q}%");
                ps.push(Box::new(like.clone()));
                ps.push(Box::new(like.clone()));
                ps.push(Box::new(like.clone()));
                ps.push(Box::new(like));
            }
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        ps.push(Box::new(f.limit as i64));
        let mut stmt = conn.prepare(&sql).context("list_items 失败")?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ps.iter().map(|p| p.as_ref())), row_to_item)
            .context("list_items 查询失败")?;
        let mut out: Vec<Item> = rows.collect::<rusqlite::Result<_>>()?;
        // 时间区间过滤（创建时间） + 按关联邮件收件时间排序（无关联邮件回退创建时间）
        out.retain(|it| range_ok(&it.created_at, f.created_after.as_deref(), f.created_before.as_deref()));
        out.retain(|it| match (&it.deadline, f.deadline_after.as_deref(), f.deadline_before.as_deref()) {
            (Some(d), after, before) => range_ok(d, after, before),
            (None, None, None) => true,
            (None, _, _) => false, // 无截止时间不满足截止区间
        });
        let recv_at = email_received_at_map(&c.mail, &out)?;
        out.sort_by(|a, b| {
            let ka = a.source_email_id.and_then(|id| recv_at.get(&id)).map(|s| s.as_str()).unwrap_or(&a.created_at);
            let kb = b.source_email_id.and_then(|id| recv_at.get(&id)).map(|s| s.as_str()).unwrap_or(&b.created_at);
            cmp_time(kb, ka).then(b.id.cmp(&a.id))
        });
        Ok(out)
    }

    /// 过期清扫：active 且截止时间已过 → expired（解析后比较，避免字符串时区误差）。
    pub fn mark_expired(&self, now: &str) -> Result<usize> {
        let now_dt = chrono::DateTime::parse_from_rfc3339(now).context("mark_expired now 非法")?;
        let actives = self.list_items(&ItemFilter {
            status: Some(ItemStatus::Active),
            limit: 10000,
            ..Default::default()
        })?;
        let mut ids = Vec::new();
        for it in actives {
            if let Some(d) = &it.deadline {
                if let Ok(dd) = chrono::DateTime::parse_from_rfc3339(d) {
                    if dd < now_dt {
                        ids.push(it.id);
                    }
                }
            }
        }
        let c = self.conns();
        let conn = &c.todo;
        let mut n = 0;
        for id in ids {
            n += conn
                .execute(
                    "UPDATE items SET status='expired', updated_at=?1 WHERE id=?2",
                    params![now, id],
                )
                .context("mark_expired 更新失败")?;
        }
        Ok(n)
    }

    pub fn distinct_parties(&self, limit: usize) -> Result<Vec<String>> {
        let c = self.conns();
        let conn = &c.todo;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT party FROM items WHERE party<>'' ORDER BY party LIMIT ?1",
            )
            .context("distinct_parties 失败")?;
        let rows = stmt
            .query_map(params![limit as i64], |r| r.get::<_, String>(0))
            .context("distinct_parties 查询失败")?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn batch_items_status(&self, ids: &[i64], status: ItemStatus) -> Result<usize> {
        let c = self.conns();
        let conn = &c.todo;
        let mut n = 0;
        for id in ids {
            n += conn
                .execute(
                    "UPDATE items SET status=?1, updated_at=?2 WHERE id=?3",
                    params![status.as_str(), now_str(), id],
                )
                .context("批量改状态失败")?;
        }
        Ok(n)
    }

    pub fn batch_items_delete(&self, ids: &[i64]) -> Result<usize> {
        let c = self.conns();
        let conn = &c.todo;
        let mut n = 0;
        for id in ids {
            n += conn
                .execute("DELETE FROM items WHERE id=?1", params![id])
                .context("批量删除事项失败")?;
            conn.execute("DELETE FROM send_log WHERE item_id=?1", params![id])
                .context("批量删除日志失败")?;
        }
        Ok(n)
    }

    // ---------- accounts ----------
    pub fn insert_account(&self, a: &MailAccount) -> Result<i64> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "INSERT INTO accounts (label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
                    smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,reminder_to,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                a.label,
                a.address,
                a.imap_host,
                a.imap_port,
                a.imap_user,
                MailAccount::obfuscate(&a.imap_password),
                a.imap_tls_insecure as i64,
                a.smtp_host,
                a.smtp_port,
                a.smtp_user,
                MailAccount::obfuscate(&a.smtp_password),
                a.smtp_tls_insecure as i64,
                a.enabled as i64,
                a.is_default as i64,
                a.reminder_to,
                a.created_at,
                a.updated_at,
            ],
        )
        .context("insert_account 失败")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_account(&self, a: &MailAccount) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "UPDATE accounts SET label=?1,address=?2,imap_host=?3,imap_port=?4,imap_user=?5,imap_password=?6,
                    imap_tls_insecure=?7,smtp_host=?8,smtp_port=?9,smtp_user=?10,smtp_password=?11,
                    smtp_tls_insecure=?12,enabled=?13,is_default=?14,reminder_to=?15,updated_at=?16 WHERE id=?17",
            params![
                a.label,
                a.address,
                a.imap_host,
                a.imap_port,
                a.imap_user,
                MailAccount::obfuscate(&a.imap_password),
                a.imap_tls_insecure as i64,
                a.smtp_host,
                a.smtp_port,
                a.smtp_user,
                MailAccount::obfuscate(&a.smtp_password),
                a.smtp_tls_insecure as i64,
                a.enabled as i64,
                a.is_default as i64,
                a.reminder_to,
                a.updated_at,
                a.id,
            ],
        )
        .context("update_account 失败")?;
        Ok(())
    }

    pub fn delete_account(&self, id: i64) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute("DELETE FROM accounts WHERE id=?1", params![id])
            .context("delete_account 失败")?;
        Ok(())
    }

    pub fn get_account(&self, id: i64) -> Result<Option<MailAccount>> {
        let c = self.conns();
        let conn = &c.mail;
        let a = conn
            .query_row(
                "SELECT id,label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
                        smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,
                        reminder_to,created_at,updated_at
                 FROM accounts WHERE id=?1",
                params![id],
                row_to_account,
            )
            .optional()
            .context("get_account 失败")?;
        Ok(a)
    }

    pub fn list_accounts(&self, enabled_only: bool) -> Result<Vec<MailAccount>> {
        let c = self.conns();
        let conn = &c.mail;
        let sql = if enabled_only {
            "SELECT id,label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
                    smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,
                    reminder_to,created_at,updated_at
             FROM accounts WHERE enabled=1 ORDER BY is_default DESC, id ASC"
        } else {
            "SELECT id,label,address,imap_host,imap_port,imap_user,imap_password,imap_tls_insecure,
                    smtp_host,smtp_port,smtp_user,smtp_password,smtp_tls_insecure,enabled,is_default,
                    reminder_to,created_at,updated_at
             FROM accounts ORDER BY is_default DESC, id ASC"
        };
        let mut stmt = conn.prepare(sql).context("list_accounts 失败")?;
        let rows = stmt
            .query_map([], row_to_account)
            .context("list_accounts 查询失败")?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_default_account(&self, id: i64) -> Result<()> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute("UPDATE accounts SET is_default=0", [])
            .context("set_default_account 失败")?;
        conn.execute("UPDATE accounts SET is_default=1 WHERE id=?1", params![id])
            .context("set_default_account 失败")?;
        Ok(())
    }

    pub fn default_account_id(&self) -> Result<Option<i64>> {
        let c = self.conns();
        let conn = &c.mail;
        let v: Option<i64> = conn
            .query_row(
                "SELECT id FROM accounts WHERE is_default=1 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("default_account_id 失败")?;
        Ok(v)
    }

    // ---------- categories ----------
    pub fn list_categories(&self) -> Result<Vec<CategoryInfo>> {
        let c = self.conns();
        let conn = &c.todo;
        let mut stmt = conn
            .prepare(
                "SELECT id,label,create_item,kind,source,created_at FROM categories ORDER BY id",
            )
            .context("list_categories 失败")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CategoryInfo {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    create_item: r.get::<_, i64>(2)? != 0,
                    kind: r.get(3)?,
                    source: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })
            .context("list_categories 查询失败")?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_category(&self, id: &str) -> Result<Option<CategoryInfo>> {
        let c = self.conns();
        let conn = &c.todo;
        let c = conn
            .query_row(
                "SELECT id,label,create_item,kind,source,created_at FROM categories WHERE id=?1",
                params![id],
                |r| {
                    Ok(CategoryInfo {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        create_item: r.get::<_, i64>(2)? != 0,
                        kind: r.get(3)?,
                        source: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .context("get_category 失败")?;
        Ok(c)
    }

    /// 插入新分类；已存在则不动。返回是否新建。
    pub fn upsert_category(
        &self,
        id: &str,
        label: &str,
        create_item: bool,
        kind: &str,
        source: &str,
    ) -> Result<bool> {
        let c = self.conns();
        let conn = &c.todo;
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM categories WHERE id=?1)",
                params![id],
                |r| r.get(0),
            )
            .context("upsert_category 查询失败")?;
        if exists != 0 {
            return Ok(false);
        }
        let kind = if kind == "todo" || kind == "notification" {
            kind.to_string()
        } else {
            default_kind(id, create_item).to_string()
        };
        conn.execute(
            "INSERT OR IGNORE INTO categories (id,label,create_item,kind,source,created_at)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, label, create_item as i64, kind, source, now_str()],
        )
        .context("upsert_category 失败")?;
        Ok(true)
    }

    // ---------- approvals ----------
    pub fn insert_approval(
        &self,
        tool_name: &str,
        payload_json: &str,
        summary: &str,
        source_email_id: Option<i64>,
    ) -> Result<i64> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "INSERT INTO approvals (tool_name,payload_json,summary,status,source_email_id,created_at)
             VALUES (?1,?2,?3,'pending',?4,?5)",
            params![tool_name, payload_json, summary, source_email_id, now_str()],
        )
        .context("insert_approval 失败")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_approvals(&self, status: Option<&str>, limit: usize) -> Result<Vec<Approval>> {
        let c = self.conns();
        let conn = &c.todo;
        let mut sql = String::from(
            "SELECT id,tool_name,payload_json,summary,status,source_email_id,decided_at,created_at
             FROM approvals WHERE 1=1",
        );
        let mut ps: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = status {
            if !s.is_empty() {
                sql.push_str(" AND status=?");
                ps.push(Box::new(s.to_string()));
            }
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        ps.push(Box::new(limit as i64));
        let mut stmt = conn.prepare(&sql).context("list_approvals 失败")?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ps.iter().map(|p| p.as_ref())), row_to_approval)
            .context("list_approvals 查询失败")?;
        let mut out: Vec<Approval> = rows.collect::<rusqlite::Result<_>>()?;
        out.sort_by(|a, b| cmp_time(&b.created_at, &a.created_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    pub fn get_approval(&self, id: i64) -> Result<Option<Approval>> {
        let c = self.conns();
        let conn = &c.todo;
        let a = conn
            .query_row(
                "SELECT id,tool_name,payload_json,summary,status,source_email_id,decided_at,created_at
                 FROM approvals WHERE id=?1",
                params![id],
                row_to_approval,
            )
            .optional()
            .context("get_approval 失败")?;
        Ok(a)
    }

    pub fn decide_approval(&self, id: i64, status: &str, decided_at: &str) -> Result<()> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "UPDATE approvals SET status=?1, decided_at=?2 WHERE id=?3",
            params![status, decided_at, id],
        )
        .context("decide_approval 失败")?;
        Ok(())
    }

    // ---------- agent_runs ----------
    pub fn insert_agent_run(&self, r: &AgentRun) -> Result<i64> {
        let c = self.conns();
        let conn = &c.mail;
        conn.execute(
            "INSERT INTO agent_runs (email_id,account_id,rounds,tool_calls,final_json,trace,status,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                r.email_id,
                r.account_id,
                r.rounds as i64,
                r.tool_calls as i64,
                r.final_json,
                r.trace,
                r.status,
                r.created_at,
            ],
        )
        .context("insert_agent_run 失败")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_agent_runs(&self, limit: usize) -> Result<Vec<AgentRun>> {
        let c = self.conns();
        let conn = &c.mail;
        let mut stmt = conn
            .prepare(
                "SELECT id,email_id,account_id,rounds,tool_calls,final_json,trace,status,created_at
                 FROM agent_runs ORDER BY id DESC LIMIT ?1",
            )
            .context("list_agent_runs 失败")?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(AgentRun {
                    id: r.get(0)?,
                    email_id: r.get(1)?,
                    account_id: r.get(2)?,
                    rounds: r.get::<_, i64>(3)? as usize,
                    tool_calls: r.get::<_, i64>(4)? as usize,
                    final_json: r.get(5)?,
                    trace: r.get(6)?,
                    status: r.get(7)?,
                    created_at: r.get(8)?,
                })
            })
            .context("list_agent_runs 查询失败")?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---------- send_log ----------
    pub fn insert_send_log(&self, log: &SendLog) -> Result<i64> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "INSERT OR IGNORE INTO send_log
             (item_id, attempt, kind, to_addr, subject, body, status, error, scheduled_at, sent_at, next_retry_at, message_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                log.item_id,
                log.attempt,
                log.kind,
                log.to_addr,
                log.subject,
                log.body,
                log.status.as_str(),
                log.error,
                log.scheduled_at,
                log.sent_at,
                log.next_retry_at,
                log.message_id,
            ],
        )
        .context("insert_send_log 失败")?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_send_log(&self, log: &SendLog) -> Result<()> {
        let c = self.conns();
        let conn = &c.todo;
        conn.execute(
            "UPDATE send_log SET attempt=?1,status=?2,error=?3,sent_at=?4,next_retry_at=?5,message_id=?6 WHERE id=?7",
            params![
                log.attempt,
                log.status.as_str(),
                log.error,
                log.sent_at,
                log.next_retry_at,
                log.message_id,
                log.id,
            ],
        )
        .context("update_send_log 失败")?;
        Ok(())
    }

    /// 按 (item_id, scheduled_at) 幂等键查找已有发送记录。
    pub fn find_send_log(&self, item_id: i64, scheduled_at: &str) -> Result<Option<SendLog>> {
        let c = self.conns();
        let conn = &c.todo;
        let log = conn
            .query_row(
                "SELECT id,item_id,attempt,kind,to_addr,subject,body,status,error,scheduled_at,sent_at,next_retry_at,message_id
                 FROM send_log WHERE item_id=?1 AND scheduled_at=?2",
                params![item_id, scheduled_at],
                row_to_log,
            )
            .optional()
            .context("find_send_log 失败")?;
        Ok(log)
    }

    pub fn list_send_logs(
        &self,
        item_id: Option<i64>,
        status: Option<SendStatus>,
        limit: usize,
    ) -> Result<Vec<SendLog>> {
        let c = self.conns();
        let conn = &c.todo;
        let mut sql = String::from(
            "SELECT id,item_id,attempt,kind,to_addr,subject,body,status,error,scheduled_at,sent_at,next_retry_at,message_id
             FROM send_log WHERE 1=1",
        );
        let mut ps: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(i) = item_id {
            sql.push_str(" AND item_id=?");
            ps.push(Box::new(i));
        }
        if let Some(s) = status {
            sql.push_str(" AND status=?");
            ps.push(Box::new(s.as_str().to_string()));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        ps.push(Box::new(limit as i64));
        let mut stmt = conn.prepare(&sql).context("list_send_logs 失败")?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ps.iter().map(|p| p.as_ref())), row_to_log)
            .context("list_send_logs 查询失败")?;
        let mut out: Vec<SendLog> = rows.collect::<rusqlite::Result<_>>()?;
        // 默认按计划发送时间倒序
        out.sort_by(|a, b| cmp_time(&b.scheduled_at, &a.scheduled_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    /// 需要处理的重试/待发日志：pending 全部 + failed 且 next_retry_at 已到。
    pub fn logs_due(&self) -> Result<Vec<SendLog>> {
        let now = now_str();
        let c = self.conns();
        let conn = &c.todo;
        let mut stmt = conn
            .prepare(
                "SELECT id,item_id,attempt,kind,to_addr,subject,body,status,error,scheduled_at,sent_at,next_retry_at,message_id
                 FROM send_log
                 WHERE status='pending'
                    OR (status='failed' AND next_retry_at IS NOT NULL AND next_retry_at<=?1)
                 ORDER BY id ASC LIMIT 100",
            )
            .context("logs_due 失败")?;
        let rows = stmt
            .query_map(params![now], row_to_log)
            .context("logs_due 查询失败")?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// 按提醒邮件 message-id 反查发送记录（回复识别）。
    pub fn find_send_log_by_message_id(&self, message_id: &str) -> Result<Option<SendLog>> {
        let c = self.conns();
        let conn = &c.todo;
        let log = conn
            .query_row(
                "SELECT id,item_id,attempt,kind,to_addr,subject,body,status,error,scheduled_at,sent_at,next_retry_at,message_id
                 FROM send_log WHERE message_id=?1 LIMIT 1",
                params![message_id],
                row_to_log,
            )
            .optional()
            .context("find_send_log_by_message_id 失败")?;
        Ok(log)
    }
}

// ---------- 过滤器 ----------

#[derive(Debug, Clone, Default)]
pub struct EmailFilter {
    pub category: Option<String>,
    pub q: Option<String>,
    /// 发件时间下界（RFC3339）
    pub since: Option<String>,
    /// 发件时间上界（RFC3339）
    pub before: Option<String>,
    pub account_id: Option<i64>,
    /// 用户标注过滤（精确匹配；`__none__`/空串 = 未标注）
    pub label: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    pub kind: Option<ItemKind>,
    pub status: Option<ItemStatus>,
    pub category: Option<String>,
    pub party: Option<String>,
    pub q: Option<String>,
    pub account_id: Option<i64>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub deadline_after: Option<String>,
    pub deadline_before: Option<String>,
    pub limit: usize,
}

// ---------- 工具函数 ----------

pub fn now_str() -> String {
    chrono::Local::now().to_rfc3339()
}

fn column_exists(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("PRAGMA table_info({table}) 失败"))?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .context("读取表结构失败")?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names.iter().any(|n| n == col))
}

/// 分类默认事务类型：通知类 → notification；其余建事务类 → todo。
fn default_kind(id: &str, create_item: bool) -> &'static str {
    if id == "notification" {
        "notification"
    } else if create_item {
        "todo"
    } else {
        "notification"
    }
}

/// 时间比较（RFC3339 解析比较；解析失败按字符串比较兜底）。
fn cmp_time(a: &str, b: &str) -> std::cmp::Ordering {
    match (
        chrono::DateTime::parse_from_rfc3339(a),
        chrono::DateTime::parse_from_rfc3339(b),
    ) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

/// [after, before] 区间过滤；解析失败视为不满足。
fn range_ok(v: &str, after: Option<&str>, before: Option<&str>) -> bool {
    if after.is_none() && before.is_none() {
        return true;
    }
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(v) else {
        return false;
    };
    if let Some(a) = after {
        if let Ok(ad) = chrono::DateTime::parse_from_rfc3339(a) {
            if dt < ad {
                return false;
            }
        }
    }
    if let Some(b) = before {
        if let Ok(bd) = chrono::DateTime::parse_from_rfc3339(b) {
            if dt > bd {
                return false;
            }
        }
    }
    true
}

/// 一次性查询一组事项关联邮件的收件时间（供 list_items 按邮件收件时间排序）。
fn email_received_at_map(conn: &Connection, items: &[Item]) -> Result<HashMap<i64, String>> {
    let ids: Vec<i64> = items.iter().filter_map(|i| i.source_email_id).collect();
    let mut map = HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("SELECT id, received_at FROM emails WHERE id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql).context("email_received_at_map 查询失败")?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter().map(|id| *id)), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .context("email_received_at_map 遍历失败")?;
    for row in rows {
        let (id, at) = row?;
        map.insert(id, at);
    }
    Ok(map)
}

// ---------- 行映射 ----------

fn row_to_email(r: &rusqlite::Row<'_>) -> rusqlite::Result<EmailRecord> {
    Ok(EmailRecord {
        id: r.get(0)?,
        uid: r.get(1)?,
        message_id: r.get(2)?,
        subject: r.get(3)?,
        from_addr: r.get(4)?,
        from_name: r.get(5)?,
        body_text: r.get(6)?,
        category: r.get(7)?,
        sent_at: r.get(8)?,
        received_at: r.get(9)?,
        item_id: r.get(10)?,
        reply_to_item_id: r.get(11)?,
        account_id: r.get(12)?,
        handled: r.get::<_, i64>(13)? != 0,
        user_label: r.get(14)?,
        user_note: r.get(15)?,
    })
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let kind: String = r.get(1)?;
    let status: String = r.get(10)?;
    Ok(Item {
        id: r.get(0)?,
        kind: ItemKind::parse(&kind).unwrap_or(ItemKind::Todo),
        title: r.get(2)?,
        party: r.get(3)?,
        event: r.get(4)?,
        category: r.get(5)?,
        deadline: r.get(6)?,
        remind_at: r.get(7)?,
        remind_policy: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
        needs_review: r.get::<_, i64>(9)? != 0,
        status: ItemStatus::parse(&status).unwrap_or(ItemStatus::Active),
        source_email_id: r.get(11)?,
        account_id: r.get(12)?,
        notes: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

fn row_to_log(r: &rusqlite::Row<'_>) -> rusqlite::Result<SendLog> {
    let status: String = r.get(7)?;
    Ok(SendLog {
        id: r.get(0)?,
        item_id: r.get(1)?,
        attempt: r.get(2)?,
        kind: r.get(3)?,
        to_addr: r.get(4)?,
        subject: r.get(5)?,
        body: r.get(6)?,
        status: SendStatus::parse(&status).unwrap_or(SendStatus::Pending),
        error: r.get(8)?,
        scheduled_at: r.get(9)?,
        sent_at: r.get(10)?,
        next_retry_at: r.get(11)?,
        message_id: r.get(12)?,
    })
}

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailAccount> {
    Ok(MailAccount {
        id: r.get(0)?,
        label: r.get(1)?,
        address: r.get(2)?,
        imap_host: r.get(3)?,
        imap_port: r.get::<_, i64>(4)? as u16,
        imap_user: r.get(5)?,
        imap_password: MailAccount::deobfuscate(&r.get::<_, String>(6)?),
        imap_tls_insecure: r.get::<_, i64>(7)? != 0,
        smtp_host: r.get(8)?,
        smtp_port: r.get::<_, i64>(9)? as u16,
        smtp_user: r.get(10)?,
        smtp_password: MailAccount::deobfuscate(&r.get::<_, String>(11)?),
        smtp_tls_insecure: r.get::<_, i64>(12)? != 0,
        enabled: r.get::<_, i64>(13)? != 0,
        is_default: r.get::<_, i64>(14)? != 0,
        reminder_to: r.get(15)?,
        created_at: r.get(16)?,
        updated_at: r.get(17)?,
    })
}

fn row_to_approval(r: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        id: r.get(0)?,
        tool_name: r.get(1)?,
        payload_json: r.get(2)?,
        summary: r.get(3)?,
        status: r.get(4)?,
        source_email_id: r.get(5)?,
        decided_at: r.get(6)?,
        created_at: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        Db::open(dir.path().join("data"), None, None).unwrap()
    }

    #[test]
    fn seed_defaults_always_inserts_configured_categories() {
        // 回归：迁移会先插入 career_promo，若种子逻辑仍以“表为空”为条件，
        // llm.yaml 的分类全集就不会落库（Agent 只能自建分类）。
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let db = Db::open(data_dir.clone(), None, None).unwrap();
        // 模拟“表里已有一条迁移插入的分类”
        db.upsert_category("career_promo", "招聘推广", false, "notification", "builtin")
            .unwrap();
        let cfg = crate::config::AppConfig::load(&write_min_config(&dir)).unwrap();
        db.seed_defaults(&cfg).unwrap();
        for id in ["todo", "interview", "assessment", "written_test", "notification", "misc"] {
            assert!(
                db.get_category(id).unwrap().is_some(),
                "种子分类 {id} 应存在"
            );
        }
    }

    /// 生成最小可加载的配置目录（llm.yaml + config.yaml），供种子测试使用。
    fn write_min_config(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let p = dir.path().to_path_buf();
        std::fs::write(
            p.join("llm.yaml"),
            "api_key: k\nbase_url: http://127.0.0.1:1\nmodel: mock\n",
        )
        .unwrap();
        std::fs::write(
            p.join("config.yaml"),
            "mail:\n  address: a@b.c\n  imap: { host: 127.0.0.1, port: 3143, user: a@b.c, password: p }\n  smtp: { host: 127.0.0.1, port: 3025, user: a@b.c, password: p }\n",
        )
        .unwrap();
        p
    }

    #[test]
    fn splits_legacy_single_database_into_mail_and_todo() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let legacy_path = data_dir.join("mail2.db");
        // 旧版单库 = 邮件域 + 待办域的表放在同一个文件里
        {
            let conn = Connection::open(&legacy_path).unwrap();
            mail_schema(&conn).unwrap();
            todo_schema(&conn).unwrap();
            conn.execute(
                "INSERT INTO emails (message_id,subject,from_addr,body_text,category,sent_at,received_at,account_id)
                 VALUES ('m1','测试邮件','a@b.c','正文','notification','2026-09-01T00:00:00+00:00','2026-09-01T01:00:00+00:00',1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO items (kind,title,party,event,category,deadline,status,source_email_id,account_id,created_at,updated_at)
                 VALUES ('todo','面试邀请','某公司','技术面试','interview','2026-09-10T14:00:00+08:00','active',1,1,'2026-09-01T01:00:00+00:00','2026-09-01T01:00:00+00:00')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO categories (id,label,create_item,kind,source,created_at)
                 VALUES ('custom_cat','自定义',0,'notification','llm','2026-09-01T01:00:00+00:00')",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO kv (k,v) VALUES ('last_uid:1','42')", []).unwrap();
        }

        let db = Db::open(data_dir.clone(), None, None).unwrap();

        // 两个新文件生成，旧文件改名保留
        assert!(data_dir.join("mail.db").exists(), "应生成 mail.db");
        assert!(data_dir.join("todo.db").exists(), "应生成 todo.db");
        assert!(!legacy_path.exists(), "旧库应被改名");
        assert!(data_dir.join("mail2.db.bak").exists(), "旧库应保留为 .bak");

        // 数据按域落位
        let emails = db
            .list_emails(&EmailFilter { limit: 10, ..Default::default() })
            .unwrap();
        assert_eq!(emails.len(), 1, "邮件应迁移到 mail.db");
        let items = db
            .list_items(&ItemFilter { limit: 10, ..Default::default() })
            .unwrap();
        assert_eq!(items.len(), 1, "待办应迁移到 todo.db");
        assert!(db.get_category("custom_cat").unwrap().is_some(), "分类应迁移到 todo.db");
        assert_eq!(
            db.kv_get("last_uid:1").unwrap().as_deref(),
            Some("42"),
            "收信游标应在 mail.db"
        );

        // 再次打开不应重复迁移
        drop(db);
        let db2 = Db::open(data_dir, None, None).unwrap();
        assert_eq!(
            db2.list_items(&ItemFilter { limit: 10, ..Default::default() })
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn migrate_legacy_adds_columns() {
        // 模拟旧库：只有旧结构 emails/items（无 sent_at/account_id 列）
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        {
            let conn = Connection::open(data_dir.join("mail2.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE emails (id INTEGER PRIMARY KEY AUTOINCREMENT, uid INTEGER,
                     message_id TEXT NOT NULL UNIQUE, subject TEXT NOT NULL DEFAULT '',
                     from_addr TEXT NOT NULL DEFAULT '', from_name TEXT NOT NULL DEFAULT '',
                     body_text TEXT NOT NULL DEFAULT '', category TEXT NOT NULL DEFAULT '',
                     received_at TEXT NOT NULL DEFAULT '', item_id INTEGER,
                     reply_to_item_id INTEGER, handled INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL DEFAULT 'todo',
                     title TEXT NOT NULL DEFAULT '', party TEXT NOT NULL DEFAULT '',
                     event TEXT NOT NULL DEFAULT '', category TEXT NOT NULL DEFAULT '',
                     deadline TEXT, remind_at TEXT, needs_review INTEGER NOT NULL DEFAULT 0,
                     status TEXT NOT NULL DEFAULT 'active', source_email_id INTEGER,
                     notes TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT '',
                     updated_at TEXT NOT NULL DEFAULT '');",
            )
            .unwrap();
        }
        // 重新打开 → 迁移补列 + 新表；旧数据不受影响
        let db = Db::open(data_dir.clone(), None, None).unwrap();
        let conn = db.conns();
        let mail = &conn.mail;
        let todo = &conn.todo;
        assert!(column_exists(mail, "emails", "sent_at").unwrap());
        assert!(column_exists(mail, "emails", "account_id").unwrap());
        assert!(column_exists(todo, "items", "account_id").unwrap());
        let n: i64 = todo
            .query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 回归：密码全链路（insert 混淆 → 读出解混淆 → endpoint 透传）后
    /// 必须与原始明文一致。16 字符合法 base64 明文密码若被二次解码会变乱码。
    #[test]
    fn account_password_roundtrip_stays_verbatim() {
        let db = test_db();
        let raw = "MDEyMzQ1Njc4OWFi";
        let a = MailAccount::from_config(&crate::config::MailConfig {
            address: "u@example.com".into(),
            imap: crate::config::Endpoint {
                host: "imap.example.com".into(),
                port: 993,
                user: "u@example.com".into(),
                password: raw.into(),
                tls_insecure: false,
            },
            smtp: crate::config::Endpoint {
                host: "smtp.example.com".into(),
                port: 465,
                user: "u@example.com".into(),
                password: raw.into(),
                tls_insecure: false,
            },
        });
        // 库内必须是混淆（base64）形态，而非明文
        let id = db.insert_account(&a).unwrap();
        // 注意：conn() 返回 MutexGuard，须在块内释放，避免跨 db.* 调用自锁死锁
        let stored_imap: String = {
            let conn = db.conns();
            conn.mail
                .query_row("SELECT imap_password FROM accounts WHERE id=?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_ne!(stored_imap, raw, "DB 中应存混淆后的密码");
        assert_eq!(MailAccount::deobfuscate(&stored_imap), raw);

        // 读出 → endpoint → 密码必须是明文原样
        let got = db.get_account(id).unwrap().unwrap();
        assert_eq!(got.imap_password, raw, "row_to_account 解混淆后应为明文");
        assert_eq!(got.imap_endpoint().password, raw);
        assert_eq!(got.smtp_endpoint().password, raw);

        // update_account 同样保持混淆边界
        let mut upd = got.clone();
        upd.imap_password = "newPass1234ABCD".into();
        db.update_account(&upd).unwrap();
        let got2 = db.get_account(id).unwrap().unwrap();
        assert_eq!(got2.imap_password, "newPass1234ABCD");
        assert_eq!(got2.imap_endpoint().password, "newPass1234ABCD");
    }

    #[test]
    fn mark_expired_only_past_deadlines() {
        let db = test_db();
        let now = now_str();
        let past = "2020-01-01T00:00:00+08:00";
        let future = "2099-01-01T00:00:00+08:00";
        for (i, d) in [Some(past), Some(future), None].iter().enumerate() {
            let it = Item {
                id: 0,
                kind: ItemKind::Todo,
                title: format!("t{i}"),
                party: String::new(),
                event: format!("e{i}"),
                category: "todo".into(),
                deadline: d.map(|s| s.to_string()),
                remind_at: None,
                remind_policy: String::new(),
                needs_review: false,
                status: ItemStatus::Active,
                source_email_id: None,
                account_id: 1,
                notes: String::new(),
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            db.insert_item(&it).unwrap();
        }
        let n = db.mark_expired(&now).unwrap();
        assert_eq!(n, 1, "仅过期事项被标记");
        let items = db
            .list_items(&ItemFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        let expired = items.iter().filter(|i| i.status == ItemStatus::Expired).count();
        assert_eq!(expired, 1);
    }

    #[test]
    fn emails_sorted_by_received_at_desc() {
        let db = test_db();
        let mut ids = Vec::new();
        // sent_at 顺序与 received_at 相反 → 验证以收件时间为准
        let rows = [
            ("2027-01-03T00:00:00+00:00", "2027-01-01T08:00:00+08:00"),
            ("2027-01-01T00:00:00+00:00", "2027-01-03T08:00:00+08:00"),
            ("2027-01-02T00:00:00+00:00", "2027-01-02T08:00:00+08:00"),
        ];
        for (i, (sent, recv)) in rows.iter().enumerate() {
            let e = EmailRecord {
                id: 0,
                uid: None,
                message_id: format!("m{i}"),
                subject: format!("s{i}"),
                from_addr: "a@b".into(),
                from_name: String::new(),
                body_text: String::new(),
                category: "misc".into(),
                sent_at: sent.to_string(),
                received_at: recv.to_string(),
                item_id: None,
                reply_to_item_id: None,
                account_id: 1,
                handled: false,
                user_label: String::new(),
                user_note: String::new(),
            };
            ids.push(db.insert_email(&e).unwrap().unwrap());
        }
        let list = db
            .list_emails(&EmailFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(list[0].received_at, "2027-01-03T08:00:00+08:00", "按收件时间倒序");
        assert_eq!(list[1].received_at, "2027-01-02T08:00:00+08:00");
        assert_eq!(list[2].received_at, "2027-01-01T08:00:00+08:00");
        assert_eq!(list[0].id, ids[1]);
    }

    #[test]
    fn items_sorted_by_email_received_at() {
        let db = test_db();
        let mk_email = |mid: &str, recv: &str| EmailRecord {
            id: 0,
            uid: None,
            message_id: mid.into(),
            subject: mid.into(),
            from_addr: "a@b".into(),
            from_name: String::new(),
            body_text: String::new(),
            category: "misc".into(),
            sent_at: String::new(),
            received_at: recv.into(),
            item_id: None,
            reply_to_item_id: None,
            account_id: 1,
            handled: false,
            user_label: String::new(),
            user_note: String::new(),
        };
        let mk_item = |title: &str, source: Option<i64>, created: &str| Item {
            id: 0,
            kind: ItemKind::Todo,
            title: title.into(),
            party: String::new(),
            event: title.into(),
            category: "todo".into(),
            deadline: None,
            remind_at: None,
            remind_policy: String::new(),
            needs_review: false,
            status: ItemStatus::Active,
            source_email_id: source,
            account_id: 1,
            notes: String::new(),
            created_at: created.into(),
            updated_at: created.into(),
        };
        let e1 = db
            .insert_email(&mk_email("m1", "2027-01-01T08:00:00+08:00"))
            .unwrap()
            .unwrap();
        let e2 = db
            .insert_email(&mk_email("m2", "2027-01-03T08:00:00+08:00"))
            .unwrap()
            .unwrap();
        // 手动事项 created_at 介于两封邮件之间（不应按创建时间排）
        db.insert_item(&mk_item("手动事项", None, "2027-01-02T08:00:00+08:00")).unwrap();
        db.insert_item(&mk_item("邮件1事项", Some(e1), "2027-01-05T08:00:00+08:00")).unwrap();
        db.insert_item(&mk_item("邮件2事项", Some(e2), "2027-01-05T08:00:00+08:00")).unwrap();

        let list = db
            .list_items(&ItemFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        let titles: Vec<&str> = list.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["邮件2事项", "手动事项", "邮件1事项"],
            "关联事项按邮件收件时间倒序，手动事项以创建时间兜底参与排序"
        );
    }

    #[test]
    fn upsert_category_once() {
        let db = test_db();
        assert!(db.upsert_category("online_test", "线上测评", true, "todo", "llm").unwrap());
        assert!(!db.upsert_category("online_test", "线上测评", true, "todo", "llm").unwrap());
        let c = db.get_category("online_test").unwrap().unwrap();
        assert_eq!(c.kind, "todo");
        assert_eq!(c.source, "llm");
    }
}
