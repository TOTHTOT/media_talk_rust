//! SIP 客户端: 配置 (SipClientConfig), 构造, 注册循环 (process_register),
//! 优雅关停 (stop/shutdown), 以及 endpoint 收包循环的 spawn 助手.

use anyhow::Result;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::Uri;
use rsipstack::transaction::Endpoint;
use rsipstack::transport::TransportLayer;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::{EndpointBuilder, rsip};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Debug)]
pub struct SipClientConfig {
    register_name: String,
    username: String,
    password: String,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
    // none 的话表示服务器自动控制
    registration_duration: Option<u32>,
}
impl SipClientConfig {
    pub fn new(
        register_name: String,
        username: String,
        password: String,
        server_addr: SocketAddr,
        client_addr: SocketAddr,
        registration_duration: Option<u32>,
    ) -> Self {
        Self {
            register_name,
            username,
            password,
            server_addr,
            client_addr,
            registration_duration,
        }
    }

    pub fn to_credential(&self) -> Credential {
        Credential {
            username: self.username.clone(),
            password: self.password.clone(),
            realm: None,
        }
    }
}

pub struct SipClient {
    config: SipClientConfig,
    pub endpoint: Endpoint,
    pub contact: Uri,
    pub dialog_layer: Arc<DialogLayer>,
    pub sip_server_uri: Uri,
    cancel_token: CancellationToken,
}
impl SipClient {
    /// `cancel_token` 由调用方（二进制的 root token）传入：cancel 即
    /// 触发优雅关停（注册循环 best-effort 注销后退出，transport 和
    /// endpoint 共用同一 token 一起停）。
    pub async fn new(
        config: SipClientConfig,
        cancel_token: CancellationToken,
    ) -> Result<Self, anyhow::Error> {
        if let SocketAddr::V4(ipv4) = config.server_addr {
            let sip_uri = format!("sip:{}@{}:{}", config.register_name, ipv4.ip(), ipv4.port());
            info!(?config, sip_uri, "server input");

            let sip_server_uri = Uri::try_from(sip_uri.as_str())?;

            let transport_layer = TransportLayer::new(cancel_token.clone());
            let connection = UdpConnection::create_connection(
                config.client_addr,
                None,
                Some(cancel_token.clone()),
            )
            .await?;
            transport_layer.add_transport(connection.into());

            let endpoint = EndpointBuilder::new()
                .with_cancel_token(cancel_token.clone())
                .with_transport_layer(transport_layer)
                .build();
            let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
            let contact = Uri {
                scheme: Some(rsip::Scheme::Sip),
                auth: Some(rsip::Auth {
                    user: config.username.clone(),
                    password: Some(config.password.clone()),
                }),
                host_with_port: endpoint
                    .get_addrs()
                    .first()
                    .ok_or_else(|| anyhow::Error::msg("no hosts found"))?
                    .addr
                    .clone(),
                params: vec![],
                headers: vec![],
            };
            Ok(Self {
                config,
                endpoint,
                contact,
                sip_server_uri,
                dialog_layer,
                cancel_token,
            })
        } else {
            anyhow::bail!("SIP server address is invalid");
        }
    }

    /// 触发优雅关停：取消 transport / endpoint / 注册循环共享的 token。
    /// 注册循环收到后会先做 best-effort 注销（Expires:0）再退出。
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    /// endpoint 收包循环甩后台：从共享 inner 重建一个 owned Endpoint
    /// 再 spawn，避免借用 client 导致 spawn 不出去
    pub fn spawn_endpoint(&self) -> tokio::task::JoinHandle<()> {
        let ep = Endpoint {
            inner: self.endpoint.inner.clone(),
        };
        tokio::spawn(async move { ep.serve().await })
    }

    /// 标准关停：取消共享 token（注册循环收到后 best-effort 注销），
    /// 再等注册循环把注销流程跑完。reg 就是调用方 Box::pin 住的
    /// process_register() future
    pub async fn shutdown(&self, reg: impl Future<Output = Result<()>>) -> Result<()> {
        self.stop();
        let r = reg.await;
        info!(result = ?r, "register loop exited");
        r
    }

    /// 被叫 URI 归一化：裸号码补全成 sip:<号码>@<服务器>，
    /// 完整 sip:/sips: URI 原样解析
    pub fn callee_uri(&self, callee: &str) -> Result<Uri> {
        let text = if callee.starts_with("sip:") || callee.starts_with("sips:") {
            callee.to_owned()
        } else {
            format!("sip:{}@{}", callee, self.config.server_addr)
        };
        Ok(Uri::try_from(text.as_str())?)
    }

    pub async fn process_register(&self) -> Result<()> {
        let mut registration = Registration::new(
            self.endpoint.inner.clone(),
            Some(self.config.to_credential()),
        );
        let mut backoff = INITIAL_BACKOFF;
        let mut registered = false;

        // 唯一取消点：cancel 即 drop 当轮 cycle，内部 register/sleep
        // 走到哪断到哪（structured cancellation），不用层层接 token
        while let Some(cycle) = self
            .cancel_token
            .run_until_cancelled(self.register_cycle(&mut registration, backoff))
            .await
        {
            match cycle? {
                Cycle::Registered => {
                    registered = true;
                    backoff = INITIAL_BACKOFF;
                }
                Cycle::Retry => backoff = next_backoff(backoff),
            }
        }

        // SIP 没有独立的注销消息：注销 = 再发一次 REGISTER 且 Expires:0
        // （RFC 3261 §10.2.2），让服务器立刻删绑定而不是等自然过期
        // （期间来电还会往死地址转发）。cancel 后 endpoint 收包循环已停，
        // 包发得出去但等不到响应，超时就走
        if registered {
            debug!("unregistering (Expires: 0)");
            let _ = tokio::time::timeout(
                UNREGISTER_TIMEOUT,
                registration.register(self.sip_server_uri.clone(), Some(0)),
            )
            .await;
        }
        debug!("registration loop exited");
        Ok(())
    }

    /// 一轮注册周期：注册一次，然后睡到下一个动作点——成功睡到
    /// 续期点（75% 有效期），失败睡到重试点（当前退避值）
    async fn register_cycle(
        &self,
        registration: &mut Registration,
        backoff: Duration,
    ) -> Result<Cycle> {
        let rsp = registration
            .register(
                self.sip_server_uri.clone(),
                self.config.registration_duration,
            )
            .await;
        let (cycle, wait) = match rsp {
            Ok(rsp) => {
                let cycle = classify_response(&rsp.status_code)?;
                let wait = match cycle {
                    Cycle::Registered => {
                        debug!("register success");
                        // 续期间隔不能直接用 registration.expires()：rsipstack 不采纳
                        // 200 OK 里的 Contact，而我们把有效期放在 Expires 头而非
                        // Contact 参数，contact 上永远没有 expires，会落到默认值 50。
                        // 显式配了 duration 就以它为准；没配才用 expires() 的默认逻辑
                        let granted = self
                            .config
                            .registration_duration
                            .unwrap_or_else(|| registration.expires());
                        refresh_after(granted)
                    }
                    Cycle::Retry => {
                        debug!(code = ?rsp.status_code, ?backoff, "register failed, retrying");
                        backoff
                    }
                };
                (cycle, wait)
            }
            Err(e) => {
                debug!(error = ?e, ?backoff, "register error, retrying");
                (Cycle::Retry, backoff)
            }
        };
        tokio::time::sleep(wait).await;
        Ok(cycle)
    }
}

/// 本机 LAN IPv4：Contact/SDP 都要写对端可达的地址，不能是 127.0.0.1
pub fn local_ipv4() -> Result<Ipv4Addr> {
    let IpAddr::V4(ip) = local_ip_address::local_ip()? else {
        anyhow::bail!("仅支持 IPv4");
    };
    Ok(ip)
}

// 退避起点 / 上限，与 ipcam-gst 重连策略一致（1s 起步指数到 30s）
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// 注销（Expires:0）等待响应的最长时间——关停是 best-effort，
/// 不能反过来把关停流程卡死。
const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(2);

/// 一轮注册周期的结果，决定主循环下一步（重置/加倍退避）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cycle {
    Registered,
    Retry,
}

/// 退避递增：加倍，封顶 MAX_BACKOFF
fn next_backoff(cur: Duration) -> Duration {
    (cur * 2).min(MAX_BACKOFF)
}

/// 续期间隔：有效期的 75%（下限 1s），必须赶在过期之前续
fn refresh_after(granted_secs: u32) -> Duration {
    Duration::from_secs((granted_secs as u64 * 3 / 4).max(1))
}

/// REGISTER 响应分类。鉴权/权限类失败（401/403/404）重试无意义，
/// 直接永久失败——401 的 digest challenge 已由 rsipstack 内部处理过，
/// 走到这还是 401 就是凭据错。其余（超时/5xx 等）都可重试。
fn classify_response(code: &rsip::StatusCode) -> Result<Cycle> {
    match code {
        rsip::StatusCode::OK => Ok(Cycle::Registered),
        rsip::StatusCode::Unauthorized
        | rsip::StatusCode::Forbidden
        | rsip::StatusCode::NotFound => {
            anyhow::bail!("register rejected permanently: {code}")
        }
        _ => Ok(Cycle::Retry),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;

    // ---- L1：纯函数（无网络） ----

    #[test]
    fn backoff_doubles_until_cap() {
        assert_eq!(next_backoff(INITIAL_BACKOFF), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(4)), Duration::from_secs(8));
        assert_eq!(next_backoff(Duration::from_secs(16)), MAX_BACKOFF);
        assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
    }

    #[test]
    fn refresh_is_three_quarters_of_granted_with_floor() {
        assert_eq!(refresh_after(3600), Duration::from_secs(2700));
        assert_eq!(refresh_after(2), Duration::from_secs(1));
        // 取整到 0 时保下限 1s，避免立即续期风暴
        assert_eq!(refresh_after(1), Duration::from_secs(1));
        assert_eq!(refresh_after(0), Duration::from_secs(1));
    }

    #[test]
    fn response_classification() {
        assert_eq!(
            classify_response(&rsip::StatusCode::OK).unwrap(),
            Cycle::Registered
        );
        // 鉴权/权限类：永久失败
        for code in [
            rsip::StatusCode::Unauthorized,
            rsip::StatusCode::Forbidden,
            rsip::StatusCode::NotFound,
        ] {
            assert!(
                classify_response(&code).is_err(),
                "{code} must be permanent"
            );
        }
        // 其余：可重试
        for code in [
            rsip::StatusCode::RequestTimeout,
            rsip::StatusCode::ServerInternalError,
            rsip::StatusCode::BusyHere,
        ] {
            assert_eq!(classify_response(&code).unwrap(), Cycle::Retry);
        }
    }

    // ---- L2：loopback 假 SIP 服务器（全自动，paused time） ----

    fn test_config(server_addr: SocketAddr) -> SipClientConfig {
        SipClientConfig::new(
            "1001".into(),
            "1001".into(),
            "pw".into(),
            server_addr,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            // 1s 有效期：续期间隔取到下限 1s，测试能快速看到 CSeq:2 续期包
            Some(1),
        )
    }

    /// 迷你假 SIP UAS：loopback UDP，REGISTER 一律回指定状态码，每个
    /// 请求全文进 channel 供断言。响应回显 Via/From/To/Call-ID/CSeq
    /// （rsipstack 事务匹配靠 Via branch + CSeq）。
    async fn fake_uas(status: rsip::StatusCode) -> (SocketAddr, mpsc::Receiver<String>) {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = sock.local_addr().unwrap();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                if !req.starts_with("REGISTER") {
                    continue;
                }
                let mut resp = format!("SIP/2.0 {status}\r\n");
                for prefix in ["Via:", "From:", "To:", "Call-ID:", "CSeq:"] {
                    if let Some(line) = req.lines().find(|l| l.starts_with(prefix)) {
                        resp.push_str(line);
                        resp.push_str("\r\n");
                    }
                }
                resp.push_str("Content-Length: 0\r\n\r\n");
                let _ = sock.send_to(resp.as_bytes(), peer).await;
                let _ = tx.send(req).await;
            }
        });
        (addr, rx)
    }

    /// stop() 后必须发出 Expires:0 注销包，且注册循环干净退出。
    /// 真实时钟（~4s）：续期 1s + 注销 2s 超时兜底都是真实等待。
    #[tokio::test]
    async fn unregister_sent_on_stop() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .try_init();
        let (server_addr, mut observed) = fake_uas(rsip::StatusCode::OK).await;
        let client = SipClient::new(test_config(server_addr), CancellationToken::new())
            .await
            .unwrap();
        let _endpoint = client.spawn_endpoint();

        let mut reg = Box::pin(client.process_register());
        // 等注册真正完成（200 OK 被处理、registered=true）再 stop：
        // 有效期 1s → 1s 后发出续期包 CSeq:2，见到它即证明注册成功。
        // （看到首个 REGISTER 就 stop 是抢跑——200 还在 dispatch 路上，
        // cancel 会先把当轮 cycle drop 掉，轮不到注销）
        loop {
            tokio::select! {
                biased;
                r = &mut reg => panic!("register loop exited early: {:?}", r),
                msg = observed.recv() => {
                    if msg.expect("REGISTER").contains("CSeq: 2") {
                        break;
                    }
                }
            }
        }

        client.stop();
        reg.await
            .expect("register loop should exit cleanly after stop");

        // 注销包在 reg 收尾期间发出，fake server 收到即塞进了 channel
        // （UDP 重传会产生重复的非注销包，逐个翻找）
        let mut saw_unregister = false;
        while let Ok(req) = observed.try_recv() {
            if req.contains("Expires: 0") {
                saw_unregister = true;
            }
        }
        assert!(saw_unregister, "stop 后应发出 Expires:0 注销包");
    }

    /// 403 是永久失败：直接报错退出，不重试、不注销（从未注册成功）。
    /// 用 CSeq 区分重传（同事务，CSeq 不变）与重试（新一轮，CSeq+1）。
    #[tokio::test]
    async fn forbidden_is_permanent() {
        let (server_addr, mut observed) = fake_uas(rsip::StatusCode::Forbidden).await;
        let client = SipClient::new(test_config(server_addr), CancellationToken::new())
            .await
            .unwrap();
        let _endpoint = client.spawn_endpoint();

        let err = client
            .process_register()
            .await
            .expect_err("403 must bail permanently");
        assert!(err.to_string().contains("permanently"), "got: {err}");

        observed.recv().await.expect("one REGISTER");
        // 首次退避是 1s——若被误分类成 Retry，3s 内一定会看到 CSeq:2 的重试
        tokio::time::sleep(Duration::from_secs(3)).await;
        while let Ok(req) = observed.try_recv() {
            assert!(req.contains("CSeq: 1"), "永久失败不应重试:\n{req}");
        }
    }
}
