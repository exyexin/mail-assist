//! IMAP 收信：拉取 UID 大于上次已处理值的全部邮件原文。
//! 993 端口 = 隐式 TLS（tokio-rustls + 系统根证书）；其余端口按明文连接（本地测试服务器）。

use anyhow::{Context, Result};
use async_imap::Client;
use futures_util::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::config::Endpoint;

#[derive(Debug, Clone)]
pub struct ImapClient {
    host: String,
    port: u16,
    user: String,
    password: String,
    inbox: String,
    tls_insecure: bool,
}

impl ImapClient {
    pub fn from_config(ep: &Endpoint) -> Self {
        ImapClient {
            host: ep.host.clone(),
            port: ep.port,
            user: ep.user.clone(),
            password: ep.password.clone(),
            inbox: "INBOX".to_string(),
            tls_insecure: ep.tls_insecure,
        }
    }

    /// 测试用构造
    pub fn new(host: &str, port: u16, user: &str, password: &str) -> Self {
        ImapClient {
            host: host.to_string(),
            port,
            user: user.to_string(),
            password: password.to_string(),
            inbox: "INBOX".to_string(),
            tls_insecure: false,
        }
    }

    /// 拉取 UID > since_uid 的邮件原文；返回 (原文列表, 当前最大 UID)。
    /// since_uid = None 时拉取全量（手动范围拉取/账户测试用）。
    pub async fn fetch_new(&self, since_uid: Option<u32>) -> Result<(Vec<Vec<u8>>, Option<u32>)> {
        let started = std::time::Instant::now();
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .with_context(|| format!("连接 IMAP {}:{} 失败", self.host, self.port))?;
        tracing::debug!(
            "IMAP TCP 连接建立: {}:{} user={} 耗时={}ms",
            self.host,
            self.port,
            self.user,
            started.elapsed().as_millis()
        );
        if self.port == 993 {
            let stream = tls_connect(&self.host, tcp, self.tls_insecure).await?;
            self.fetch_with_stream(stream, since_uid).await
        } else {
            self.fetch_with_stream(tcp, since_uid).await
        }
    }

    /// 连接测试：登录 + SELECT INBOX + 登出（不拉取邮件）。
    pub async fn test(&self) -> Result<()> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .with_context(|| format!("连接 IMAP {}:{} 失败", self.host, self.port))?;
        if self.port == 993 {
            let stream = tls_connect(&self.host, tcp, self.tls_insecure).await?;
            let mut session = Client::new(stream)
                .login(&self.user, &self.password)
                .await
                .map_err(|(e, _)| anyhow::anyhow!("IMAP 登录失败: {e}"))?;
            session
                .select(&self.inbox)
                .await
                .map_err(|e| anyhow::anyhow!("IMAP SELECT {} 失败: {e}", self.inbox))?;
            session.logout().await.ok();
        } else {
            let mut session = Client::new(tcp)
                .login(&self.user, &self.password)
                .await
                .map_err(|(e, _)| anyhow::anyhow!("IMAP 登录失败: {e}"))?;
            session
                .select(&self.inbox)
                .await
                .map_err(|e| anyhow::anyhow!("IMAP SELECT {} 失败: {e}", self.inbox))?;
            session.logout().await.ok();
        }
        Ok(())
    }

    async fn fetch_with_stream<S>(
        &self,
        stream: S,
        since_uid: Option<u32>,
    ) -> Result<(Vec<Vec<u8>>, Option<u32>)>
    where
        S: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send,
    {
        let started = std::time::Instant::now();
        let client = Client::new(stream);
        let mut session = client
            .login(&self.user, &self.password)
            .await
            .map_err(|(e, _)| anyhow::anyhow!("IMAP 登录失败: {e}"))?;
        tracing::info!(
            "IMAP 登录成功: {}:{} user={} 耗时={}ms",
            self.host,
            self.port,
            self.user,
            started.elapsed().as_millis()
        );
        session
            .select(&self.inbox)
            .await
            .map_err(|e| anyhow::anyhow!("IMAP SELECT {} 失败: {e}", self.inbox))?;
        tracing::debug!("IMAP SELECT {} 完成: {}:{}", self.inbox, self.host, self.port);

        let seq = match since_uid {
            Some(u) => format!("{}:*", u + 1),
            None => "1:*".to_string(),
        };
        tracing::debug!("发起 IMAP UID FETCH 范围 {seq}（since_uid={since_uid:?}）");
        let msgs = session
            .uid_fetch(seq, "(RFC822)")
            .await
            .map_err(|e| anyhow::anyhow!("IMAP UID FETCH 失败: {e}"))?;

        let mut raws = Vec::new();
        let mut max_uid: Option<u32> = None;
        let mut stream = msgs;
        while let Some(msg) = stream.try_next().await? {
            if let Some(uid) = msg.uid {
                max_uid = Some(max_uid.map_or(uid, |m: u32| m.max(uid)));
            }
            if let Some(body) = msg.body() {
                raws.push(body.to_vec());
                tracing::trace!(
                    "IMAP 消息: uid={:?} bytes={}",
                    msg.uid,
                    body.len()
                );
            }
        }
        drop(stream); // 释放对 session 的可变借用
        session.logout().await.ok();
        tracing::info!(
            "IMAP 拉取完成: {}:{} fetched={} max_uid={:?} 耗时={}ms",
            self.host,
            self.port,
            raws.len(),
            max_uid,
            started.elapsed().as_millis()
        );
        Ok((raws, max_uid))
    }
}

/// 隐式 TLS（993）：rustls + 系统根证书；tls_insecure = true 时保留加密但跳过证书域名校验。
async fn tls_connect(
    host: &str,
    tcp: TcpStream,
    tls_insecure: bool,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    // 依赖图中同时存在 ring（lettre）与 aws-lc-rs（tokio-rustls 默认），
    // rustls 无法自动判定 → 显式安装 ring provider。
    static CRYPTO_INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = CRYPTO_INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let builder = rustls::ClientConfig::builder();
    let config = if tls_insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        let certs = rustls_native_certs::load_native_certs();
        for cert in certs.certs {
            roots.add(cert).ok();
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| anyhow::anyhow!("非法 IMAP 主机名: {host}"))?;
    connector
        .connect(server_name, tcp)
        .await
        .with_context(|| format!("IMAP TLS 握手失败 {host}:993"))
}

/// 不校验证书的验证器（仅 tls_insecure = true 时使用；加密通道保留）。
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
