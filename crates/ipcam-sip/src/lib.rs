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
use tokio::select;
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
    cancel_token: CancellationToken
}
impl SipClient {
    pub async fn new(config: SipClientConfig) -> Result<Self, anyhow::Error> {
        if let SocketAddr::V4(ipv4) = config.server_addr {
            let sip_uri = format!("sip:{}@{}:{}", config.register_name, ipv4.ip(), ipv4.port());
            debug!(?config, sip_uri, "server input");

            let sip_server_uri = Uri::try_from(sip_uri.as_str())?;
            let cancel_token = CancellationToken::new();

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
                    .clone()
                    .into(),
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

    async fn process_register(&self) -> Result<()> {
        let mut registration = Registration::new(
            self.endpoint.inner.clone(),
            Some(self.config.to_credential()),
        );
        loop {
            select! {
                _ = self.cancel_token.cancelled() => {
                    debug!("cancel registration");
                }
            }
            let rsp = registration
                .register(
                    self.sip_server_uri.clone(),
                    self.config.registration_duration,
                )
                .await?;
            debug!(?rsp, "register success");
            if rsp.status_code != rsip::StatusCode::OK {
                anyhow::bail!("register failed");
            }
            tokio::time::sleep(Duration::from_secs(registration.expires() as u64)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_ip_address::local_ip;
    use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
    use std::str::FromStr;
    use tracing::error;
    use tracing_test::traced_test;

    #[test]
    #[traced_test]
    fn connect_sip_server() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        if let IpAddr::V4(local_ip) = local_ip().expect("local ip") {
            let ipv4 = local_ip.to_string();
            let ip_port = format!("{}:{}", ipv4, 5063);
            let config = SipClientConfig {
                register_name: "1001".to_string(),
                username: "1001".to_string(),
                password: "changeme".to_string(),
                server_addr: SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(192, 168, 11, 17),
                    5062,
                )),
                client_addr: SocketAddr::V4(SocketAddrV4::from_str(&ip_port).expect("url")),
                registration_duration: Some(10),
            };

            rt.block_on(async move {
                let client = SipClient::new(config).await.unwrap();
                tokio::select! {
                    _ = client.endpoint.serve() =>{
                        debug!("SIP server connected");
                    }

                    r = client.process_register() =>{
                        debug!(result = ?r, "SIP server process register");
                    }
                }
            });
        } else {
            error!("only support ipv4 for now");
        }
    }
}
