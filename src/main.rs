//! mail2 入口：serve / fetch-once / check-once / init

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use mail2::clock::{Clock, SystemClock};
use mail2::config::AppConfig;
use mail2::mail::{ImapClient, SmtpClient};
use mail2::{api, checker, llm, pipeline, scheduler};

#[derive(Parser)]
#[command(
    name = "mail2",
    version,
    about = "邮件管理工具：收信 + DeepSeek 分类 + 待办/DDL 提醒 + Web 控制台"
)]
struct Cli {
    /// 配置目录（含 llm.yaml 与 config.yaml / config），缺省当前目录
    #[arg(short, long, default_value = ".")]
    config_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动服务：IMAP 轮询 + 调度器 + Web/API（默认）
    Serve,
    /// 立即拉取并处理一次新邮件
    FetchOnce {
        /// 只处理最新 N 封（游标不推进；测试真实邮箱时建议配合 --no-check）
        #[arg(long)]
        limit: Option<usize>,
        /// 不触发待办检查（禁止任何 SMTP 发送；测试真实邮箱时必须使用）
        #[arg(long)]
        no_check: bool,
    },
    /// 立即执行一次待办检查（漏发/失败重试）
    CheckOnce,
    /// 生成 config.example / llm.yaml.example 模板
    Init,
}

fn init_tracing(data_dir: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // 控制台输出（stdout）
    let console = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);
    let registry = tracing_subscriber::registry()
        .with(console)
        .with(filter.clone());

    // 文件输出：<data_dir>/logs/ 下按天轮转（mail2.log.YYYY-MM-DD）
    let log_dir = data_dir.join("logs");
    match std::fs::create_dir_all(&log_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(&log_dir, "mail2.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_filter(filter);
            registry.with(file).init();
            tracing::info!(
                "日志输出目录: {}（控制台 + 按天轮转文件，过滤级别见 RUST_LOG）",
                log_dir.display()
            );
            Some(guard)
        }
        Err(e) => {
            eprintln!(
                "警告: 创建日志目录 {} 失败，仅控制台输出: {e:#}",
                log_dir.display()
            );
            registry.init();
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = AppConfig::load(&cli.config_dir)?;

    match cli.command {
        Command::Init => {
            write_examples(&cli.config_dir)?;
            println!("已生成 config.example 与 llm.yaml.example");
            Ok(())
        }
        Command::FetchOnce { .. } | Command::CheckOnce | Command::Serve => {
            // 日志初始化依赖 data_dir（文件日志目录），故在配置加载后进行
            let _log_guard = init_tracing(&cfg.data_dir);
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let app = mail2::App::build(cfg, clock)?;
            match cli.command {
                Command::FetchOnce { limit, no_check } => {
                    let account_id = app.db.default_account_id()?.unwrap_or(1);
                    let report = pipeline::process_new_mails_opts(
                        &app.db,
                        &ImapClient::from_config(&app.cfg.mail.imap),
                        &app.smtp,
                        &app.llm,
                        &app.cfg,
                        app.clock.as_ref(),
                        account_id,
                        pipeline::ProcessOpts {
                            limit,
                            run_check: !no_check,
                            ..Default::default()
                        },
                    )
                    .await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    Ok(())
                }
                Command::CheckOnce => {
                    let report =
                        checker::run_check(&app.db, &app.smtp, &app.cfg, app.clock.as_ref())
                            .await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    Ok(())
                }
                Command::Serve => serve(app, &cli.config_dir).await,
                _ => unreachable!(),
            }
        }
    }
}

async fn serve(app: mail2::App, config_dir: &std::path::Path) -> Result<()> {
    let listen = app.cfg.listen.clone();
    let poll_secs = app.cfg.poll_interval_secs;
    let web_dir = config_dir.join("web").to_string_lossy().to_string();

    // 后台：调度循环
    {
        let db = app.db.clone();
        let smtp = app.smtp.clone();
        let cfg = app.cfg.clone();
        let clock = app.clock.clone();
        tokio::spawn(async move { scheduler::run_loop(db, smtp, cfg, clock).await });
    }

    // 后台：IMAP 轮询（遍历全部启用账户；账户列表每轮重新读取，增删改即时生效）
    {
        let db = app.db.clone();
        let cfg = app.cfg.clone();
        let clock = app.clock.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
            loop {
                interval.tick().await;
                let accounts = match db.list_accounts(true) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("读取账户列表失败: {e:#}");
                        continue;
                    }
                };
                for acct in accounts {
                    let llm = match llm::LlmClient::new(&cfg.llm) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("LLM 客户端构建失败: {e:#}");
                            continue;
                        }
                    };
                    let imap = ImapClient::from_config(&acct.imap_endpoint());
                    let smtp = match SmtpClient::from_config(&acct.smtp_endpoint(), &acct.address)
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("账户 {} SMTP 客户端构建失败: {e:#}", acct.address);
                            continue;
                        }
                    };
                    let started = std::time::Instant::now();
                    match pipeline::process_new_mails_opts(
                        &db,
                        &imap,
                        &smtp,
                        &llm,
                        &cfg,
                        clock.as_ref(),
                        acct.id,
                        pipeline::ProcessOpts::default(),
                    )
                    .await
                    {
                        Ok(report) => {
                            tracing::info!(
                                "账户 {} 轮询完成: fetched={} new={} classified={} items_created={} items_updated={} replies={} tool_rounds={} approvals={} reminders_sent={} reminders_failed={} missed={} retries={} expired={} 耗时={}ms",
                                acct.address,
                                report.fetched,
                                report.new_emails,
                                report.classified,
                                report.items_created,
                                report.items_updated,
                                report.replies_handled,
                                report.tool_rounds,
                                report.approvals_created,
                                report.reminders_sent,
                                report.reminders_failed,
                                report.missed_compensated,
                                report.retries,
                                report.expired,
                                started.elapsed().as_millis()
                            );
                        }
                        Err(e) => {
                            tracing::warn!("账户 {} 邮件处理循环出错: {e:#}", acct.address);
                        }
                    }
                }
            }
        });
    }

    let state = api::AppState::with_components(
        app.cfg.clone(),
        app.db.clone(),
        app.smtp.clone(),
        app.llm.clone(),
        app.clock.clone(),
        web_dir,
    );
    let router = api::router(state);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("监听 {listen} 失败"))?;
    tracing::info!("mail2 服务启动: http://{listen} （Web 控制台 + /api/v1）");
    axum::serve(listener, router)
        .await
        .context("服务运行失败")?;
    Ok(())
}

fn write_examples(dir: &std::path::Path) -> Result<()> {
    let cfg_example = include_str!("../config.example");
    let llm_example = include_str!("../llm.yaml.example");
    let c = dir.join("config.example");
    let l = dir.join("llm.yaml.example");
    if !c.exists() {
        std::fs::write(&c, cfg_example).context("写 config.example 失败")?;
    }
    if !l.exists() {
        std::fs::write(&l, llm_example).context("写 llm.yaml.example 失败")?;
    }
    Ok(())
}
