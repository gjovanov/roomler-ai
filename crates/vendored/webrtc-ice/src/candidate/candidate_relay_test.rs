use std::result::Result;
use std::time::Duration;

use tokio::net::UdpSocket;
use turn::auth::AuthHandler;

use super::*;
use crate::agent::agent_config::AgentConfig;
use crate::agent::agent_vnet_test::{connect_with_vnet, on_connected};
use crate::agent::Agent;
use crate::error::Error;
use crate::url::{ProtoType, SchemeType, Url};

pub(crate) struct OptimisticAuthHandler;

impl AuthHandler for OptimisticAuthHandler {
    fn auth_handle(
        &self,
        _username: &str,
        _realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        Ok(turn::auth::generate_auth_key(
            "username",
            "webrtc.rs",
            "password",
        ))
    }
}

//use std::io::Write;

#[tokio::test]
async fn test_relay_only_connection() -> Result<(), Error> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, log::LevelFilter::Trace)
    .init();*/

    let server_listener = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let server_port = server_listener.local_addr()?.port();

    let server = turn::server::Server::new(turn::server::config::ServerConfig {
        realm: "webrtc.rs".to_owned(),
        auth_handler: Arc::new(OptimisticAuthHandler {}),
        conn_configs: vec![turn::server::config::ConnConfig {
            conn: server_listener,
            relay_addr_generator: Box::new(turn::relay::relay_none::RelayAddressGeneratorNone {
                address: "127.0.0.1".to_owned(),
                net: Arc::new(util::vnet::net::Net::new(None)),
            }),
        }],
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await?;

    let cfg0 = AgentConfig {
        network_types: supported_network_types(),
        urls: vec![Url {
            scheme: SchemeType::Turn,
            host: "127.0.0.1".to_owned(),
            username: "username".to_owned(),
            password: "password".to_owned(),
            port: server_port,
            proto: ProtoType::Udp,
        }],
        candidate_types: vec![CandidateType::Relay],
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(cfg0).await?);
    let (a_notifier, mut a_connected) = on_connected();
    a_agent.on_connection_state_change(a_notifier);

    let cfg1 = AgentConfig {
        network_types: supported_network_types(),
        urls: vec![Url {
            scheme: SchemeType::Turn,
            host: "127.0.0.1".to_owned(),
            username: "username".to_owned(),
            password: "password".to_owned(),
            port: server_port,
            proto: ProtoType::Udp,
        }],
        candidate_types: vec![CandidateType::Relay],
        ..Default::default()
    };

    let b_agent = Arc::new(Agent::new(cfg1).await?);
    let (b_notifier, mut b_connected) = on_connected();
    b_agent.on_connection_state_change(b_notifier);

    connect_with_vnet(&a_agent, &b_agent).await?;

    let _ = a_connected.recv().await;
    let _ = b_connected.recv().await;

    a_agent.close().await?;
    b_agent.close().await?;
    server.close().await?;

    Ok(())
}

/// roomler (FR-148 P0): a relay candidate names the transport and the scheme
/// of the TURN URL its allocation was made over, which the agent's carrier
/// line reads; a candidate of another type names none.
#[tokio::test]
async fn a_relay_candidate_names_what_carries_its_allocation() -> Result<(), Error> {
    let server_listener = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let server_port = server_listener.local_addr()?.port();
    let server = turn::server::Server::new(turn::server::config::ServerConfig {
        realm: "webrtc.rs".to_owned(),
        auth_handler: Arc::new(OptimisticAuthHandler {}),
        conn_configs: vec![turn::server::config::ConnConfig {
            conn: server_listener,
            relay_addr_generator: Box::new(turn::relay::relay_none::RelayAddressGeneratorNone {
                address: "127.0.0.1".to_owned(),
                net: Arc::new(util::vnet::net::Net::new(None)),
            }),
        }],
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await?;
    let agent = Agent::new(AgentConfig {
        network_types: supported_network_types(),
        urls: vec![Url {
            scheme: SchemeType::Turn,
            host: "127.0.0.1".to_owned(),
            username: "username".to_owned(),
            password: "password".to_owned(),
            port: server_port,
            proto: ProtoType::Udp,
        }],
        candidate_types: vec![CandidateType::Relay],
        ..Default::default()
    })
    .await?;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));
    agent.on_candidate(Box::new(
        move |c: Option<Arc<dyn Candidate + Send + Sync>>| {
            let done_tx = Arc::clone(&done_tx);
            Box::pin(async move {
                if c.is_none() {
                    done_tx.lock().await.take();
                }
            })
        },
    ));
    agent.gather_candidates()?;
    let _ = done_rx.recv().await;

    let candidates = agent.get_local_candidates().await?;
    assert!(!candidates.is_empty(), "no relay candidate was gathered");
    for c in &candidates {
        assert_eq!(c.candidate_type(), CandidateType::Relay, "{c}");
        assert_eq!(
            c.relay_transport(),
            Some((ProtoType::Udp, SchemeType::Turn)),
            "{c}"
        );
    }
    let host = candidate_host::CandidateHostConfig {
        base_config: CandidateBaseConfig {
            network: "udp".to_owned(),
            address: "192.0.2.1".to_owned(),
            port: 19216,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    assert_eq!(host.relay_transport(), None, "{host}");

    agent.close().await?;
    server.close().await?;
    Ok(())
}
