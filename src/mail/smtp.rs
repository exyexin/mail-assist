//! SMTP 发信（lettre）：465 = 隐式 TLS（relay）、587 = STARTTLS、其它 = 明文（本地测试服务器）。

use anyhow::{Context, Result};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};

use crate::config::Endpoint;

#[derive(Debug, Clone)]
pub struct SmtpClient {
    from: String,
    transport: SmtpTransport,
}

impl SmtpClient {
    /// from_config：SMTP 服务器配置 + 发件人地址（通常 = 邮箱地址）。
    pub fn from_config(ep: &Endpoint, address: &str) -> Result<Self> {
        let tls_params = || -> Result<TlsParameters> {
            if ep.tls_insecure {
                TlsParameters::builder(ep.host.clone())
                    .dangerous_accept_invalid_certs(true)
                    .dangerous_accept_invalid_hostnames(true)
                    .build()
                    .context("构建 SMTP TLS 参数失败")
            } else {
                TlsParameters::new(ep.host.clone()).context("构建 SMTP TLS 参数失败")
            }
        };
        let builder = if ep.port == 465 {
            lettre::SmtpTransport::builder_dangerous(&ep.host)
                .port(ep.port)
                .tls(Tls::Wrapper(tls_params()?))
        } else if ep.port == 587 {
            lettre::SmtpTransport::builder_dangerous(&ep.host)
                .port(ep.port)
                .tls(Tls::Required(tls_params()?))
        } else {
            lettre::SmtpTransport::builder_dangerous(&ep.host).port(ep.port)
        };
        let transport = builder
            .credentials(Credentials::new(ep.user.clone(), ep.password.clone()))
            .build();
        let from = if !address.trim().is_empty() {
            address.to_string()
        } else if !ep.user.trim().is_empty() {
            ep.user.clone()
        } else {
            "mail2@localhost".to_string()
        };
        Ok(SmtpClient { from, transport })
    }

    /// 测试用构造（明文端口，如 greenmail 3025）
    pub fn new_plain(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let transport = lettre::SmtpTransport::builder_dangerous(host)
            .port(port)
            .credentials(Credentials::new(user.to_string(), password.to_string()))
            .build();
        Ok(SmtpClient {
            from: if user.is_empty() {
                "mail2@localhost".into()
            } else {
                user.to_string()
            },
            transport,
        })
    }

    /// 发送邮件；message_id 形如 "<xxx@mail2>"。
    pub fn send(&self, to: &str, subject: &str, body: &str, message_id: &str) -> Result<()> {
        let from = self.from.clone();
        let email = Message::builder()
            .from(from.parse().context("发件人地址非法")?)
            .to(to.parse().context("收件人地址非法")?)
            .subject(subject.to_string())
            .message_id(Some(message_id.to_string()))
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())
            .context("构建邮件失败")?;
        self.transport
            .send(&email)
            .map(|_| ())
            .context("SMTP 发送失败")
    }
}
