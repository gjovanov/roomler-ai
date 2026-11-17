//! Roomler patch (FR-148 P4): the carrier rank's cells.
//!
//! The field case: a remote-desktop agent (the controlled side) selects its
//! mesh pair first, its relay candidate arrives after that selection, and the
//! mesh link underneath is relayed. With the rank, every candidate only the
//! mesh carries ranks below every relay off the mesh, the agent keeps checking
//! the pairs its follow gate would follow, and its first check from the relay
//! opens the TURN permission the browser's checks need.

use std::net::{IpAddr, Ipv4Addr};

use stun::agent::TransactionId;
use stun::attributes::ATTR_USERNAME;
use stun::fingerprint::FINGERPRINT;
use stun::integrity::MessageIntegrity;
use stun::message::{Message, BINDING_REQUEST};
use stun::textattrs::Username;
use util::vnet::*;

use super::agent_gather::{
    host_priority, overlay_host_priority_override, relay_priority, srflx_priority,
};
use super::agent_selector::{outranks_on_a_real_path, should_follow_renomination};
use super::agent_vnet_test::*;
use super::*;
use crate::candidate::candidate_base::*;
use crate::candidate::candidate_host::*;
use crate::candidate::candidate_relay::*;
use crate::candidate::candidate_server_reflexive::*;
use crate::control::AttrControlling;
use crate::priority::PriorityAttr;
use crate::use_candidate::UseCandidateAttr;

type Cand = Arc<dyn Candidate + Send + Sync>;

/// Today's priority of a host candidate anywhere in 100.64.0.0/10: type 90.
const TYPE_90: u32 = (1 << 24) * 90 + (1 << 8) * 65535 + 255;
const UDP_RELAY: u32 = (1 << 8) * 65535 + 255;
const TLS_RELAY: u32 = (1 << 8) * 32767 + 255;

/// The rank as the agent sets it for a relayed mesh link: the mesh's own v4
/// block (here 100.65.4.0/22) and the derived-v6 ULA, never the whole /10.
fn mesh_last() -> CarrierRank {
    CarrierRank {
        mesh_last: Some(Box::new(|ip: IpAddr| match ip {
            IpAddr::V4(v4) => u32::from(v4) & 0xffff_fc00 == u32::from(Ipv4Addr::new(100, 65, 4, 0)),
            IpAddr::V6(v6) => v6.segments()[..6] == [0xfd72, 0x6f6f, 0x6d6c, 0, 0, 0],
        })),
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn host(address: &str, port: u16, priority: u32) -> Cand {
    Arc::new(
        CandidateHostConfig {
            base_config: CandidateBaseConfig {
                network: "udp".to_owned(),
                address: address.to_owned(),
                port,
                component: 1,
                priority,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()
        .unwrap(),
    )
}

fn srflx(address: &str, port: u16, priority: u32) -> Cand {
    Arc::new(
        CandidateServerReflexiveConfig {
            base_config: CandidateBaseConfig {
                network: "udp".to_owned(),
                address: address.to_owned(),
                port,
                component: 1,
                priority,
                ..Default::default()
            },
            rel_addr: "0.0.0.0".to_owned(),
            rel_port: 0,
        }
        .new_candidate_server_reflexive()
        .unwrap(),
    )
}

fn relay(address: &str, port: u16, priority: u32) -> Cand {
    Arc::new(
        CandidateRelayConfig {
            base_config: CandidateBaseConfig {
                network: "udp".to_owned(),
                address: address.to_owned(),
                port,
                component: 1,
                priority,
                ..Default::default()
            },
            rel_addr: "0.0.0.0".to_owned(),
            rel_port: 0,
            ..Default::default()
        }
        .new_candidate_relay()
        .unwrap(),
    )
}

/// A browser candidate's priority as libwebrtc computes it (component 1).
fn chrome(type_preference: u32, local_preference: u32) -> u32 {
    (type_preference << 24) | (local_preference << 8) | 255
}

/// Done means 1: the priorities, set where each candidate is made.
#[test]
fn candidates_only_the_mesh_carries_rank_last_and_tls_relays_rank_below_udp() {
    let rank = mesh_last();
    let r = Some(&rank);
    let (mesh_v4, mesh_v6) = (ip("100.65.4.34"), ip("fd72:6f6f:6d6c::6441:422"));
    let public = ip("203.0.113.7");
    // In the CGNAT /10 but not in the mesh's blocks: an LTE modem's address.
    let cgnat = ip("100.80.1.2");

    assert_eq!(host_priority(r, &mesh_v4, 1), 255, "a mesh host");
    assert_eq!(host_priority(r, &mesh_v6, 1), 255, "a mesh host on the ULA");
    assert_eq!(srflx_priority(r, &mesh_v4, 1), 255, "a srflx mapped to the mesh");
    assert_eq!(
        relay_priority(r, &public, "100.65.4.33", false, 1),
        255,
        "a relay on a mesh-addressed TURN server"
    );
    assert_eq!(
        relay_priority(r, &public, "[fd72:6f6f:6d6c::6441:421]", false, 1),
        255
    );
    assert_eq!(
        relay_priority(r, &mesh_v4, "turn.example.org", true, 1),
        255,
        "a relay relayed at a mesh address"
    );
    // The candidates carry it: the override is the candidate's priority.
    assert_eq!(host("100.65.4.34", 1, host_priority(r, &mesh_v4, 1)).priority(), 255);
    assert_eq!(srflx("100.65.4.34", 2, srflx_priority(r, &mesh_v4, 1)).priority(), 255);

    // TURN over TLS ranks below TURN over UDP on the agent's own leg.
    let udp = relay("203.0.113.7", 3, relay_priority(r, &public, "turn.example.org", false, 1));
    let tls = relay("203.0.113.7", 4, relay_priority(r, &public, "turn.example.org", true, 1));
    assert_eq!((udp.priority(), tls.priority()), (UDP_RELAY, TLS_RELAY));
    // ... and so it does with the rank on and the mesh link direct, where the
    // mesh host keeps today's type 90. (Compared with today's function, not
    // the constant: `overlay_priority_tests` flips its env escape hatch.)
    let direct = CarrierRank::default();
    assert_eq!(
        host_priority(Some(&direct), &mesh_v4, 1),
        overlay_host_priority_override(&mesh_v4, 1),
        "type 90 kept"
    );
    assert_eq!(relay_priority(Some(&direct), &public, "100.65.4.33", true, 1), TLS_RELAY);

    // An address of the /10 outside the blocks keeps its ordinary priority.
    assert_eq!(host_priority(r, &cgnat, 1), overlay_host_priority_override(&cgnat, 1));
    assert_ne!(host_priority(r, &cgnat, 1), 255);
    assert_eq!(srflx_priority(r, &cgnat, 1), 0, "computed: 1,694,498,815");
    assert_eq!(relay_priority(r, &cgnat, "100.80.1.9", false, 1), 0);

    // Without the rank every candidate is made exactly as before.
    for addr in [mesh_v4, mesh_v6, cgnat, public] {
        assert_eq!(host_priority(None, &addr, 1), overlay_host_priority_override(&addr, 1));
        assert_eq!(srflx_priority(None, &addr, 1), 0, "{addr}");
        assert_eq!(relay_priority(None, &addr, "100.65.4.33", true, 1), 0, "{addr}");
    }
}

/// Done means 2: every pair with a ranked-last candidate ranks below every
/// pair with a relay off the mesh, for both roles, down to a browser relay
/// over TLS at local preference 1.
#[test]
fn every_ranked_last_pair_ranks_below_every_pair_with_a_relay_off_the_mesh() {
    let browser = [
        chrome(126, 65535), // host, its TUN's too
        chrome(110, 65535), // prflx
        chrome(100, 65535), // srflx
        chrome(2, 65535),   // relay over UDP
        chrome(1, 65535),   // relay over TCP
        chrome(0, 1),       // relay over TLS, the lowest it computes
    ];
    let pair = |agent: u32, b: u32, agent_controls: bool| {
        CandidatePair::new(host("100.65.4.34", 1, agent), host("192.0.2.9", 2, b), agent_controls)
            .priority()
    };
    for agent_controls in [false, true] {
        let ranked_last = browser
            .iter()
            .map(|b| pair(255, *b, agent_controls))
            .max()
            .unwrap();
        let off_mesh_relay = browser
            .iter()
            .flat_map(|b| [pair(UDP_RELAY, *b, agent_controls), pair(TLS_RELAY, *b, agent_controls)])
            .min()
            .unwrap();
        assert!(
            ranked_last < off_mesh_relay,
            "agent controls: {agent_controls}: {ranked_last} !< {off_mesh_relay}"
        );
    }
    // Without the rank the mesh pair outranks every TURN pair: today's order.
    assert!(pair(TYPE_90, chrome(126, 65535), false) > pair(UDP_RELAY, chrome(100, 65535), false));
}

/// The upward test both the follow gate and the checks use.
#[test]
fn outranks_on_a_real_path_needs_a_real_remote_and_a_strictly_higher_pair() {
    let selected = CandidatePair::new(host("100.65.4.34", 1, 255), host("100.65.4.33", 2, chrome(126, 65535)), false);
    let turn = |remote: Cand| CandidatePair::new(relay("203.0.113.7", 3, UDP_RELAY), remote, false);
    assert!(outranks_on_a_real_path(&turn(host("192.0.2.9", 4, chrome(100, 65535))), &selected));
    assert!(
        !outranks_on_a_real_path(&turn(host("100.65.4.33", 5, chrome(126, 65535))), &selected),
        "a remote on the overlay range"
    );
    assert!(
        !outranks_on_a_real_path(&turn(host("4a5b6c7d.local", 6, chrome(126, 65535))), &selected),
        "a name is overlay-possible"
    );
    let sibling = CandidatePair::new(host("100.65.4.34", 7, 255), host("100.65.4.33", 2, chrome(126, 65535)), false);
    assert!(!outranks_on_a_real_path(&sibling, &selected), "equal is not above");
}

/// Done means 4: the follow gate, on the field's renomination.
#[test]
fn a_renomination_from_a_ranked_last_pair_to_a_turn_pair_is_followed_only_with_the_rank() {
    let rank = mesh_last();
    for with_rank in [true, false] {
        let r = with_rank.then_some(&rank);
        let mesh_host = host("100.65.4.34", 1, host_priority(r, &ip("100.65.4.34"), 1));
        let viewer_tun = host("100.65.4.33", 2, 0);
        // The current pair is alive: no stale-pair failover decides this.
        viewer_tun.seen(false);
        let current = CandidatePair::new(mesh_host, viewer_tun, false);
        let turn = CandidatePair::new(
            relay("203.0.113.7", 3, relay_priority(r, &ip("203.0.113.7"), "turn.example.org", false, 1)),
            srflx("198.51.100.4", 4, 0),
            false,
        );
        assert_eq!(
            should_follow_renomination(&turn, &current),
            with_rank,
            "rank {with_rank}: {current} -> {turn}"
        );
    }
}

/// A pair's state and how many checks it has sent.
async fn checks_of(a: &Agent, local: &Cand, remote: &Cand) -> (u8, u16) {
    let p = a.internal.find_pair(local, remote).await.unwrap();
    (
        p.state.load(Ordering::SeqCst),
        p.binding_request_count.load(Ordering::SeqCst),
    )
}

/// Done means 3: what the controlled agent checks after its first selection.
#[tokio::test]
async fn after_its_selection_the_agent_checks_only_pairs_it_would_follow() -> Result<()> {
    let rank = mesh_last();
    for with_rank in [true, false] {
        // The same candidates both times, ranked: only the agent's own rank
        // differs, so the second pass shows the checks are the rank's alone.
        let r = Some(&rank);
        let a = Agent::new(AgentConfig {
            carrier_rank: Arc::new(with_rank.then(mesh_last)),
            ..Default::default()
        })
        .await?;
        let mesh = ip("100.65.4.34");
        let mesh_host = host("100.65.4.34", 5000, host_priority(r, &mesh, 1));
        let mesh_srflx = srflx("100.65.4.34", 5001, srflx_priority(r, &mesh, 1));
        let turn = relay("203.0.113.7", 6000, relay_priority(r, &ip("203.0.113.7"), "turn.example.org", false, 1));
        let viewer_tun = host("100.65.4.33", 7000, 0);
        let viewer_srflx = srflx("198.51.100.4", 7001, 0);
        viewer_tun.seen(false);
        a.internal
            .set_selected_pair(Some(Arc::new(CandidatePair::new(
                Arc::clone(&mesh_host),
                Arc::clone(&viewer_tun),
                false,
            ))))
            .await;
        // Above the selection, real-path remote: the TURN pair.
        a.internal.add_pair(Arc::clone(&turn), Arc::clone(&viewer_srflx)).await;
        // Above it, but the remote is the viewer's TUN.
        a.internal.add_pair(Arc::clone(&turn), Arc::clone(&viewer_tun)).await;
        // Real-path remote, but below the selection.
        a.internal.add_pair(Arc::clone(&mesh_srflx), Arc::clone(&viewer_srflx)).await;

        let (waiting, in_progress, failed) = (
            CandidatePairState::Waiting as u8,
            CandidatePairState::InProgress as u8,
            CandidatePairState::Failed as u8,
        );

        a.internal.contact_candidates().await;
        let up = checks_of(&a, &turn, &viewer_srflx).await;
        if with_rank {
            assert_eq!(up, (in_progress, 1), "checked within one contact of existing");
        } else {
            assert_eq!(up, (waiting, 0), "today: nothing after the first selection");
        }
        assert_eq!(checks_of(&a, &turn, &viewer_tun).await, (waiting, 0), "a mesh remote");
        assert_eq!(
            checks_of(&a, &mesh_srflx, &viewer_srflx).await,
            (waiting, 0),
            "below the selection"
        );

        // Retried at the checking cadence, and bounded as every check is.
        for _ in 0..8 {
            a.internal.contact_candidates().await;
        }
        let up = checks_of(&a, &turn, &viewer_srflx).await;
        if with_rank {
            assert_eq!(up, (failed, 8), "max_binding_requests + 1 sends, then failed");
        } else {
            assert_eq!(up, (waiting, 0));
        }
        a.close().await?;
    }
    Ok(())
}

const MESH_LAN: &str = "100.65.4.1/24";
const VIEWER_TUN_IP: &str = "100.65.4.33";
const AGENT_MESH_IP: &str = "100.65.4.34";
const LAN_PUBLIC_IP: &str = "27.1.1.1";

struct MeshVNet {
    wan: Arc<Mutex<router::Router>>,
    viewer: Arc<net::Net>,
    agent: Arc<net::Net>,
    server: turn::server::Server,
}

/// The mesh, as a LAN both ends sit on, behind a full-cone NAT to a WAN where
/// the TURN server (`add_vnet_stun`, which also answers STUN) runs.
async fn build_mesh_vnet() -> Result<MeshVNet> {
    let wan = Arc::new(Mutex::new(router::Router::new(router::RouterConfig {
        cidr: "0.0.0.0/0".to_owned(),
        ..Default::default()
    })?));
    let wnet = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ip: VNET_STUN_SERVER_IP.to_owned(),
        ..Default::default()
    })));
    connect_net2router(&wnet, &wan).await?;
    let full_cone = nat::NatType {
        mapping_behavior: nat::EndpointDependencyType::EndpointIndependent,
        filtering_behavior: nat::EndpointDependencyType::EndpointIndependent,
        ..Default::default()
    };
    let lan = Arc::new(Mutex::new(router::Router::new(router::RouterConfig {
        static_ips: vec![LAN_PUBLIC_IP.to_owned()],
        cidr: MESH_LAN.to_owned(),
        nat_type: Some(full_cone),
        ..Default::default()
    })?));
    let viewer = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: vec![VIEWER_TUN_IP.to_owned()],
        ..Default::default()
    })));
    let agent = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: vec![AGENT_MESH_IP.to_owned()],
        ..Default::default()
    })));
    connect_net2router(&viewer, &lan).await?;
    connect_net2router(&agent, &lan).await?;
    connect_router2router(&lan, &wan).await?;
    start_router(&wan).await?;
    let server = add_vnet_stun(wnet).await?;
    Ok(MeshVNet {
        wan,
        viewer,
        agent,
        server,
    })
}

async fn remote_at(agent: &Agent, address: &str, port: u16) -> Option<Cand> {
    let remotes = agent.internal.remote_candidates.lock().await;
    remotes
        .values()
        .flatten()
        .find(|c| c.address() == address && c.port() == port)
        .cloned()
}

async fn pair_state(agent: &Agent, local: &Cand, remote: &Cand) -> Option<u8> {
    let checklist = agent.internal.agent_conn.checklist.lock().await;
    checklist
        .iter()
        .find(|p| {
            p.local.equal(&**local)
                && p.remote.address() == remote.address()
                && p.remote.port() == remote.port()
        })
        .map(|p| p.state.load(Ordering::SeqCst))
}

async fn selected_local(agent: &Agent) -> Option<String> {
    agent
        .internal
        .agent_conn
        .get_selected_pair()
        .map(|p| format!("{}:{}", p.local.address(), p.local.port()))
}

/// The browser (controlling) nominates a pair, as Chrome's USE-CANDIDATE.
async fn nominate(browser: &Agent, local: &Cand, remote: &Cand) {
    let (username, remote_pwd) = {
        let ufrag_pwd = browser.internal.ufrag_pwd.lock().await;
        (
            format!("{}:{}", ufrag_pwd.remote_ufrag, ufrag_pwd.local_ufrag),
            ufrag_pwd.remote_pwd.clone(),
        )
    };
    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::<UseCandidateAttr>::default(),
        Box::new(AttrControlling(browser.internal.tie_breaker.load(Ordering::SeqCst))),
        Box::new(PriorityAttr(local.priority())),
        Box::new(MessageIntegrity::new_short_term_integrity(remote_pwd)),
        Box::new(FINGERPRINT),
    ])
    .unwrap();
    browser.internal.send_binding_request(&msg, local, remote).await;
}

/// Polls `check` every 20 ms until it holds or `limit` passes.
async fn within<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let until = Instant::now() + limit;
    loop {
        if check().await {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Done means 5, in process: two agents and a TURN server on the vnet. The
/// agent's relay candidate arrives after its first selection, as in the field.
async fn turn_pair_after_the_first_selection(with_rank: bool) -> Result<()> {
    let v = build_mesh_vnet().await?;
    let browser = Arc::new(
        Agent::new(AgentConfig {
            urls: vec![Url {
                scheme: SchemeType::Stun,
                host: VNET_STUN_SERVER_IP.to_owned(),
                port: VNET_STUN_SERVER_PORT,
                proto: ProtoType::Udp,
                ..Default::default()
            }],
            network_types: supported_network_types(),
            multicast_dns_mode: MulticastDnsMode::Disabled,
            net: Some(Arc::clone(&v.viewer)),
            ..Default::default()
        })
        .await?,
    );
    // The agent has no STUN reach (CORPLAP-2's own case), only its mesh host.
    let agent = Arc::new(
        Agent::new(AgentConfig {
            network_types: supported_network_types(),
            multicast_dns_mode: MulticastDnsMode::Disabled,
            net: Some(Arc::clone(&v.agent)),
            carrier_rank: Arc::new(with_rank.then(mesh_last)),
            ..Default::default()
        })
        .await?,
    );
    let (agent_notifier, mut agent_connected) = on_connected();
    let (browser_notifier, mut browser_connected) = on_connected();
    agent.on_connection_state_change(agent_notifier);
    browser.on_connection_state_change(browser_notifier);
    // The agent accepts (controlled) and the browser dials (controlling).
    let (_agent_conn, _browser_conn) = connect_with_vnet(&agent, &browser).await?;
    let _ = agent_connected.recv().await;
    let _ = browser_connected.recv().await;

    let first = agent.internal.agent_conn.get_selected_pair().expect("a selection");
    assert_eq!(first.local.address(), AGENT_MESH_IP, "{first}");
    assert_eq!(first.local.priority() == 255, with_rank, "ranked last: {first}");
    let mesh_selection = selected_local(&agent).await;

    // The relay candidate, after the first selection.
    Agent::gather_candidates_relay(
        vec![Url {
            scheme: SchemeType::Turn,
            host: VNET_STUN_SERVER_IP.to_owned(),
            port: VNET_STUN_SERVER_PORT,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            proto: ProtoType::Udp,
        }],
        Arc::clone(&v.agent),
        Arc::clone(&agent.internal),
    )
    .await;
    let added = Instant::now();
    let turn = agent
        .get_local_candidates()
        .await?
        .into_iter()
        .find(|c| c.candidate_type() == CandidateType::Relay)
        .expect("the TURN allocation");
    assert_eq!(turn.priority(), UDP_RELAY);
    // Trickled to the browser, as the agent's rc:ice does.
    browser.add_remote_candidate(&(Arc::new(unmarshal_candidate(&turn.marshal())?) as Cand))?;
    let viewer_srflx = browser
        .get_local_candidates()
        .await?
        .into_iter()
        .find(|c| c.candidate_type() == CandidateType::ServerReflexive)
        .expect("the browser's srflx");
    let turn_at_browser = {
        let (b, a, p) = (&browser, turn.address(), turn.port());
        assert!(within(Duration::from_secs(1), || async { remote_at(b, &a, p).await.is_some() }).await);
        remote_at(&browser, &turn.address(), turn.port()).await.unwrap()
    };

    // Chrome pings every pair it has, the new one too.
    let chrome = {
        let (browser, local, remote) = (
            Arc::clone(&browser),
            Arc::clone(&viewer_srflx),
            Arc::clone(&turn_at_browser),
        );
        tokio::spawn(async move {
            loop {
                browser.internal.ping_candidate(&local, &remote).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };

    let succeeded = Some(CandidatePairState::Succeeded as u8);
    if with_rank {
        assert!(
            within(Duration::from_secs(1), || async {
                pair_state(&agent, &turn, &viewer_srflx).await == succeeded
            })
            .await,
            "the TURN pair validates within 1 s of the relay candidate ({:?} after)",
            added.elapsed()
        );
        nominate(&browser, &viewer_srflx, &turn_at_browser).await;
        let to = Some(format!("{}:{}", turn.address(), turn.port()));
        assert!(
            within(Duration::from_secs(1), || async { selected_local(&agent).await == to }).await,
            "the agent follows the nomination off its ranked-last pair"
        );
    } else {
        // The browser's checks to the relay address die at the TURN server: no
        // permission covers them, and the agent never sends from its relay.
        assert!(
            !within(Duration::from_millis(2500), || async {
                pair_state(&agent, &turn, &viewer_srflx).await == succeeded
            })
            .await,
            "today the TURN pair never validates"
        );
        nominate(&browser, &viewer_srflx, &turn_at_browser).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(selected_local(&agent).await, mesh_selection, "the nomination never arrives");
    }

    chrome.abort();
    agent.close().await?;
    browser.close().await?;
    v.server.close().await?;
    v.wan.lock().await.stop().await?;
    Ok(())
}

#[tokio::test]
async fn with_the_rank_a_turn_pair_after_the_first_selection_validates_and_is_followed() -> Result<()> {
    turn_pair_after_the_first_selection(true).await
}

#[tokio::test]
async fn without_the_rank_a_turn_pair_after_the_first_selection_never_validates() -> Result<()> {
    turn_pair_after_the_first_selection(false).await
}
