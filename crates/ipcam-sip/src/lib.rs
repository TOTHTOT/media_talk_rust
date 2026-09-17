use anyhow::Result;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::Uri;
use rsipstack::transaction::Endpoint;
use rsipstack::transport::TransportLayer;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::{EndpointBuilder, rsip};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::debug;

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
            debug!(?config, sip_uri, "server input");

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

    // 退避起点 / 上限，与 ipcam-gst 重连策略一致（1s 起步指数到 30s）
    const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    /// 注销（Expires:0）等待响应的最长时间——关停是 best-effort，
    /// 不能反过来把关停流程卡死。cancel 后 endpoint 收包循环已停，
    /// 注销包发得出去但大概率等不到响应，超时就走。
    const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(2);

    /// 触发优雅关停：取消 transport / endpoint / 注册循环共享的 token。
    /// 注册循环收到后会先做 best-effort 注销（Expires:0）再退出。
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    pub async fn process_register(&self) -> Result<()> {
        let mut registration = Registration::new(
            self.endpoint.inner.clone(),
            Some(self.config.to_credential()),
        );
        let mut backoff = Self::INITIAL_BACKOFF;
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
                    backoff = Self::INITIAL_BACKOFF;
                }
                Cycle::Retry => backoff = (backoff * 2).min(Self::MAX_BACKOFF),
            }
        }

        // 退出时再次注册但是时间设置0表示注销
        if registered {
            debug!("unregistering (Expires: 0)");
            let _ = tokio::time::timeout(
                Self::UNREGISTER_TIMEOUT,
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
            Ok(rsp) if rsp.status_code == rsip::StatusCode::OK => {
                debug!("register success");
                let granted = self
                    .config
                    .registration_duration
                    .unwrap_or_else(|| registration.expires());
                let refresh = Duration::from_secs((granted as u64 * 3 / 4).max(1));
                (Cycle::Registered, refresh)
            }
            Ok(rsp) => {
                let code = rsp.status_code;
                // 鉴权/权限类失败重试无意义, 直接放弃, 401 的 challenge
                // 已由 rsipstack 内部处理过, 走到这还是 401 就是凭据错
                if matches!(
                    code,
                    rsip::StatusCode::Unauthorized
                        | rsip::StatusCode::Forbidden
                        | rsip::StatusCode::NotFound
                ) {
                    anyhow::bail!("register rejected permanently: {code}");
                }
                debug!(?code, ?backoff, "register failed, retrying");
                (Cycle::Retry, backoff)
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

/// 一轮注册周期的结果，决定主循环下一步（重置/加倍退避）
enum Cycle {
    Registered,
    Retry,
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_ip_address::local_ip;
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
    use std::str::FromStr;
    use tokio::select;
    use tracing::warn;
    use tracing_test::traced_test;

    // 实机测试：连真实 SIP 服务器（192.168.1.17:5062），注册成功后
    // 循环续期直到 Ctrl+C。会阻塞整个测试套件，默认忽略，
    // 手动跑：cargo test -p ipcam-sip -- --ignored --nocapture
    #[test]
    #[ignore]
    #[traced_test]
    fn connect_sip_server() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let IpAddr::V4(local_ip) = local_ip().expect("local ip") else {
            warn!("only support ipv4 for now");
            return;
        };
        let ipv4 = local_ip.to_string();
        let ip_port = format!("{}:{}", ipv4, 5063);
        let config = SipClientConfig {
            register_name: "1001".to_string(),
            username: "1001".to_string(),
            password: "changeme".to_string(),
            server_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 11, 17), 5062)),
            client_addr: SocketAddr::V4(SocketAddrV4::from_str(&ip_port).expect("url")),
            registration_duration: Some(10),
        };

        rt.block_on(async move {
            let token = CancellationToken::new();
            let client = SipClient::new(config, token).await.unwrap();
            let mut reg = Box::pin(client.process_register());
            select! {
                _ = client.endpoint.serve() => {
                    debug!("SIP endpoint exited");
                }
                r = &mut reg => {
                    debug!(result = ?r, "SIP register loop exited");
                }
                _ = tokio::signal::ctrl_c() => {
                    debug!("ctrl+c received, stopping (unregister)");
                    client.stop();
                    let r = reg.await;
                    debug!(result = ?r, "SIP register loop exited");
                }
            }
        });
    }
}
