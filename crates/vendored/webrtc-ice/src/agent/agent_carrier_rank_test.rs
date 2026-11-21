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

use super::agent_gather::{host_priority, relay_priority, srflx_priority};
use super::agent_selector::{
    outranks_off_the_mesh, outranks_on_a_real_path, should_follow_renomination,
};
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

/// The mesh's own addresses: its v4 block (here 100.65.4.0/22) and the
/// derived-v6 ULA, never the whole /10.
fn mesh_blocks() -> MeshAddrFn {
    Box::new(|ip: IpAddr| match ip {
        IpAddr::V4(v4) => u32::from(v4) & 0xffff_fc00 == u32::from(Ipv4Addr::new(100, 65, 4, 0)),
        IpAddr::V6(v6) => v6.segments()[..6] == [0xfd72, 0x6f6f, 0x6d6c, 0, 0, 0],
    })
}

/// The rank as the agent sets it for a relayed mesh link under `prio` and `1`.
fn mesh_last() -> CarrierRank {
    CarrierRank {
        mesh_last: Some(mesh_blocks()),
        route: None,
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

/// What today's overlay-host override answers for an address of the /10 or the
/// ULA: type 90, or 0 while `overlay_priority_tests` holds its escape hatch on.
/// That env is process-wide and the cells run in parallel, so no cell here
/// compares two reads of it.
fn todays_overlay_host(priority: u32) -> bool {
    priority == TYPE_90 || priority == 0
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
    // mesh host keeps today's type 90.
    let direct = CarrierRank::default();
    assert!(todays_overlay_host(host_priority(Some(&direct), &mesh_v4, 1)), "type 90 kept");
    assert_eq!(srflx_priority(Some(&direct), &mesh_v4, 1), 0);
    assert_eq!(relay_priority(Some(&direct), &public, "100.65.4.33", true, 1), TLS_RELAY);
    assert_eq!(relay_priority(Some(&direct), &mesh_v4, "100.65.4.33", false, 1), 0);

    // An address of the /10 outside the blocks keeps its ordinary priority.
    assert!(todays_overlay_host(host_priority(r, &cgnat, 1)));
    assert_eq!(srflx_priority(r, &cgnat, 1), 0, "computed: 1,694,498,815");
    assert_eq!(relay_priority(r, &cgnat, "100.80.1.9", false, 1), 0);
    assert_eq!(relay_priority(r, &cgnat, "turn.example.org", true, 1), TLS_RELAY);

    // Without the rank every candidate is made exactly as before.
    for addr in [mesh_v4, mesh_v6, cgnat] {
        assert!(todays_overlay_host(host_priority(None, &addr, 1)), "{addr}");
    }
    assert_eq!(host_priority(None, &public, 1), 0);
    for addr in [mesh_v4, mesh_v6, cgnat, public] {
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

/// A STUN and TURN server the agent reaches over the mesh.
const MESH_SERVER_IP: &str = "100.65.4.1";

/// Done means 1, through the gather itself: a STUN and TURN server on a mesh
/// address. The host on the mesh, the srflx whose STUN answer maps it to the
/// mesh, and the relay that server allocates (relayed at an address off the
/// mesh, so its server alone decides) are all made at 255, and so are the
/// lines they are signalled with.
#[tokio::test]
async fn a_gather_through_a_mesh_addressed_server_makes_every_candidate_at_255() -> Result<()> {
    let lan = Arc::new(Mutex::new(router::Router::new(router::RouterConfig {
        cidr: MESH_LAN.to_owned(),
        ..Default::default()
    })?));
    let server_net = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: vec![MESH_SERVER_IP.to_owned()],
        ..Default::default()
    })));
    let agent_net = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: vec![AGENT_MESH_IP.to_owned()],
        ..Default::default()
    })));
    connect_net2router(&server_net, &lan).await?;
    connect_net2router(&agent_net, &lan).await?;
    start_router(&lan).await?;
    let conn = server_net
        .bind(SocketAddr::new(ip(MESH_SERVER_IP), VNET_STUN_SERVER_PORT))
        .await?;
    let server = turn::server::Server::new(turn::server::config::ServerConfig {
        conn_configs: vec![turn::server::config::ConnConfig {
            conn,
            relay_addr_generator: Box::new(turn::relay::relay_static::RelayAddressGeneratorStatic {
                relay_address: ip("203.0.113.7"),
                address: "0.0.0.0".to_owned(),
                net: Arc::clone(&server_net),
            }),
        }],
        realm: "webrtc.rs".to_owned(),
        auth_handler: Arc::new(TestAuthHandler::new()),
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await?;

    let url = |scheme| Url {
        scheme,
        host: MESH_SERVER_IP.to_owned(),
        port: VNET_STUN_SERVER_PORT,
        username: "user".to_owned(),
        password: "pass".to_owned(),
        proto: ProtoType::Udp,
    };
    let agent = Agent::new(AgentConfig {
        urls: vec![url(SchemeType::Stun), url(SchemeType::Turn)],
        network_types: vec![NetworkType::Udp4],
        multicast_dns_mode: MulticastDnsMode::Disabled,
        net: Some(Arc::clone(&agent_net)),
        carrier_rank: Arc::new(Some(mesh_last())),
        ..Default::default()
    })
    .await?;
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let done_tx = Arc::new(Mutex::new(Some(done_tx)));
    agent.on_candidate(Box::new(move |c: Option<Cand>| {
        let done_tx = Arc::clone(&done_tx);
        Box::pin(async move {
            if c.is_none() {
                done_tx.lock().await.take();
            }
        })
    }));
    agent.gather_candidates()?;
    let _ = done_rx.recv().await;

    let candidates = agent.get_local_candidates().await?;
    for typ in [
        CandidateType::Host,
        CandidateType::ServerReflexive,
        CandidateType::Relay,
    ] {
        let c = candidates
            .iter()
            .find(|c| c.candidate_type() == typ)
            .unwrap_or_else(|| panic!("no {typ} among {} candidates", candidates.len()));
        assert_eq!(c.priority(), 255, "{c}");
        assert_eq!(unmarshal_candidate(&c.marshal())?.priority(), 255, "{}", c.marshal());
    }
    let relay = candidates
        .iter()
        .find(|c| c.candidate_type() == CandidateType::Relay)
        .unwrap();
    assert_eq!(relay.address(), "203.0.113.7", "relayed off the mesh");

    agent.close().await?;
    server.close().await?;
    lan.lock().await.stop().await?;
    Ok(())
}

// FR-148 P4 step 2: the pair's route at the opening. The browser is the
// controlling side and selects as Chrome does: the first pair its own check
// validates, at once (`basic_ice_controller.cc`: `HandleInitialSelectDampening`
// selects at once unless a field trial is set), then nominates it
// (SEMI_AGGRESSIVE nominates while nothing is selected). The agent selects the
// first nomination of a pair it has validated itself.

/// The agent's own address off the mesh: the field's LAN host (shape A's local).
const AGENT_LAN_IP: &str = "192.168.1.10";

/// What the far TURN does in a cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FarTurn {
    /// Allocates for both ends.
    Up,
    /// Refuses both ends' credentials: both relay gathers end at once, empty.
    Refuses,
    /// Every packet to it is dropped: no relay gather ends within a cell.
    Blackholed,
}

/// The rank the agent runs in the opening cells: the mesh's own block ranked
/// last, and the route rank with a hold of `hold` at most. (Before step 2:
/// `mesh_last()` alone, today's `prio`.)
fn route_rank(hold: Duration) -> CarrierRank {
    CarrierRank {
        route: Some(RouteRank {
            hold,
            mesh: mesh_blocks(),
        }),
        ..mesh_last()
    }
}

/// The agent's first selection, how long after the checks started, the
/// agent's route read of it then (`Agent::route_read`), and every pair the
/// agent had formed by then, as (local address, remote address).
struct Opening {
    local: String,
    remote: String,
    remote_typ: CandidateType,
    after: Duration,
    read: Option<RouteRead>,
    pairs: Vec<(String, String)>,
}

impl Opening {
    /// Did the agent form a pair from `local` to `remote` (addresses)?
    fn formed(&self, local: &str, remote: &str) -> bool {
        self.pairs.iter().any(|(l, r)| l == local && r == remote)
    }
}

impl std::fmt::Display for Opening {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local {} remote {} ({}) after {:?} read {:?} pairs {:?}",
            self.local, self.remote, self.remote_typ, self.after, self.read, self.pairs
        )
    }
}

/// One opening's network: every address on one flat net (see
/// [`opening_on_a_flat_net`]).
struct FlatNet<'a> {
    agent_ips: &'a [&'a str],
    browser_ips: &'a [&'a str],
    far_turn: FarTurn,
    /// Do the browser's candidates reach the agent? Without, the agent learns
    /// the browser's addresses only from its checks: as peer-reflexive remotes.
    browser_trickles: bool,
}

/// One session's opening on a flat network, where every address reaches every
/// other: as in the field, where a check from the agent's LAN socket to a mesh
/// address reaches it (the destination picks the route, not the socket's
/// source). The browser sits at the viewer's mesh address, and its host
/// candidate there stands for the viewer's relay on the mesh (a pair is classed
/// by its remote's address, not its type). The far TURN is at
/// `VNET_STUN_SERVER_IP`, and both ends allocate on it, as in the field's TURN
/// sessions. Every hop costs 10 ms, so a TURN pair needs more time to validate
/// than a direct one. Candidates trickle both ways, as `rc:ice` does, and the
/// checks start before either gather ends.
async fn opening_on_a_flat_net(
    agent_ips: &[&str],
    rank: CarrierRank,
    far_turn: FarTurn,
) -> Result<Opening> {
    opening(
        FlatNet {
            agent_ips,
            browser_ips: &[VIEWER_TUN_IP],
            far_turn,
            browser_trickles: true,
        },
        rank,
    )
    .await
}

/// [`opening_on_a_flat_net`] on any [`FlatNet`].
async fn opening(flat: FlatNet<'_>, rank: CarrierRank) -> Result<Opening> {
    let FlatNet {
        agent_ips,
        browser_ips,
        far_turn,
        browser_trickles,
    } = flat;
    let router = Arc::new(Mutex::new(router::Router::new(router::RouterConfig {
        cidr: "0.0.0.0/0".to_owned(),
        min_delay: Duration::from_millis(10),
        ..Default::default()
    })?));
    if far_turn == FarTurn::Blackholed {
        let turn_ip = ip(VNET_STUN_SERVER_IP);
        router
            .lock()
            .await
            .add_chunk_filter(Box::new(move |c: &(dyn chunk::Chunk + Send + Sync)| {
                c.get_destination_ip() != turn_ip
            }))
            .await;
    }
    let turn_net = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ip: VNET_STUN_SERVER_IP.to_owned(),
        ..Default::default()
    })));
    let viewer = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: browser_ips.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    })));
    let agent_net = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: agent_ips.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    })));
    connect_net2router(&turn_net, &router).await?;
    connect_net2router(&viewer, &router).await?;
    connect_net2router(&agent_net, &router).await?;
    start_router(&router).await?;
    let server = add_vnet_stun(turn_net).await?;

    let turn = Url {
        scheme: SchemeType::Turn,
        host: VNET_STUN_SERVER_IP.to_owned(),
        port: VNET_STUN_SERVER_PORT,
        username: "user".to_owned(),
        password: if far_turn == FarTurn::Refuses { "wrong" } else { "pass" }.to_owned(),
        proto: ProtoType::Udp,
    };
    let never = Some(Duration::from_secs(3600));
    let browser = Arc::new(
        Agent::new(AgentConfig {
            urls: vec![turn.clone()],
            network_types: vec![NetworkType::Udp4],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            net: Some(Arc::clone(&viewer)),
            // The Chrome stand-in below decides; this agent never nominates by
            // itself, and keeps checking an unanswered pair as Chrome does.
            host_acceptance_min_wait: never,
            srflx_acceptance_min_wait: never,
            prflx_acceptance_min_wait: never,
            relay_acceptance_min_wait: never,
            max_binding_requests: Some(100),
            ..Default::default()
        })
        .await?,
    );
    let agent = Arc::new(
        Agent::new(AgentConfig {
            urls: vec![turn],
            network_types: vec![NetworkType::Udp4],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            net: Some(Arc::clone(&agent_net)),
            carrier_rank: Arc::new(Some(rank)),
            ..Default::default()
        })
        .await?,
    );
    for (from, to, trickles) in [(&agent, &browser, true), (&browser, &agent, browser_trickles)] {
        let to = Arc::clone(to);
        from.on_candidate(Box::new(move |c: Option<Cand>| {
            let to = Arc::clone(&to);
            Box::pin(async move {
                if let Some(c) = c.filter(|_| trickles) {
                    if let Ok(c) = unmarshal_candidate(&c.marshal()) {
                        let _ = to.add_remote_candidate(&(Arc::new(c) as Cand));
                    }
                }
            })
        }));
    }

    let (agent_ufrag, agent_pwd) = agent.get_local_user_credentials().await;
    let (browser_ufrag, browser_pwd) = browser.get_local_user_credentials().await;
    agent.gather_candidates()?;
    browser.gather_candidates()?;
    let started = Instant::now();
    let (_agent_cancel, agent_cancel_rx) = mpsc::channel(1);
    let (_browser_cancel, browser_cancel_rx) = mpsc::channel(1);
    let accepting = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move {
            let _ = agent.accept(agent_cancel_rx, browser_ufrag, browser_pwd).await;
        })
    };
    let dialing = {
        let browser = Arc::clone(&browser);
        tokio::spawn(async move {
            let _ = browser.dial(browser_cancel_rx, agent_ufrag, agent_pwd).await;
        })
    };
    // Chrome: the first pair its own check validates, nominated at once and
    // again every 100 ms, as long as the session lives.
    let chrome = {
        let browser = Arc::clone(&browser);
        tokio::spawn(async move {
            let mut chosen: Option<(Cand, Cand)> = None;
            loop {
                if chosen.is_none() {
                    let checklist = browser.internal.agent_conn.checklist.lock().await;
                    chosen = checklist
                        .iter()
                        .find(|p| p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8)
                        .map(|p| (Arc::clone(&p.local), Arc::clone(&p.remote)));
                }
                if let Some((local, remote)) = &chosen {
                    nominate(&browser, local, remote).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        })
    };

    let mut opening = None;
    while started.elapsed() < Duration::from_secs(8) {
        if let Some(p) = agent.internal.agent_conn.get_selected_pair() {
            let after = started.elapsed();
            let read = agent.route_read(
                p.local.priority(),
                p.local.component(),
                &p.local.address(),
                &p.remote.address(),
            );
            let pairs = {
                let checklist = agent.internal.agent_conn.checklist.lock().await;
                checklist
                    .iter()
                    .map(|q| (q.local.address(), q.remote.address()))
                    .collect()
            };
            opening = Some(Opening {
                local: format!("{}:{}", p.local.address(), p.local.port()),
                remote: format!("{}:{}", p.remote.address(), p.remote.port()),
                remote_typ: p.remote.candidate_type(),
                after,
                read,
                pairs,
            });
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    chrome.abort();
    accepting.abort();
    dialing.abort();
    agent.close().await?;
    browser.close().await?;
    server.close().await?;
    router.lock().await.stop().await?;
    opening.ok_or_else(|| Error::Other("no selection within 8 s".to_owned()))
}

/// Shape A: the agent's LAN host paired with the viewer's relay on the mesh
/// validates first, the browser selects it, and the session rides the mesh
/// although the far TURN allocated. Its local is not ranked last, so the
/// rank never sees it.
#[tokio::test]
async fn shape_a_the_opening_rides_turn_not_the_lan_host_paired_into_the_mesh() -> Result<()> {
    let o = opening_on_a_flat_net(&[AGENT_LAN_IP], route_rank(Duration::from_secs(3)), FarTurn::Up).await?;
    println!("shape A: {o}");
    assert!(
        o.remote.starts_with(&format!("{VNET_STUN_SERVER_IP}:")),
        "the opening rides the far TURN: {o}"
    );
    Ok(())
}

/// Shape B: the agent's mesh host, ranked last, paired with the viewer's relay
/// on the mesh validates first, and the browser selects the first pair it
/// validates whatever its priority.
#[tokio::test]
async fn shape_b_the_opening_rides_turn_not_the_ranked_last_mesh_pair() -> Result<()> {
    let o = opening_on_a_flat_net(&[AGENT_MESH_IP], route_rank(Duration::from_secs(3)), FarTurn::Up).await?;
    println!("shape B: {o}");
    assert!(
        o.remote.starts_with(&format!("{VNET_STUN_SERVER_IP}:")),
        "the opening rides the far TURN: {o}"
    );
    Ok(())
}

/// AC23, the far TURN refuses: no relay off the mesh can come, so the session
/// opens on the mesh at once, far inside the hold's bound.
#[tokio::test]
async fn ac23_a_refusing_turn_opens_on_the_mesh_at_once() -> Result<()> {
    let o = opening_on_a_flat_net(&[AGENT_MESH_IP], route_rank(Duration::from_secs(10)), FarTurn::Refuses).await?;
    println!("AC23 refused: {o}");
    assert!(o.remote.starts_with(&format!("{VIEWER_TUN_IP}:")), "on the mesh: {o}");
    assert!(o.after < Duration::from_secs(2), "inside the 10 s bound: {o}");
    Ok(())
}

/// AC23, the far TURN blackholed: neither relay gather ends, so only the
/// hold's bound lets the mesh open, and it opens within AC23's 5 s.
#[tokio::test]
async fn ac23_a_blackholed_turn_opens_on_the_mesh_within_5_s() -> Result<()> {
    let o = opening_on_a_flat_net(
        &[AGENT_MESH_IP],
        route_rank(Duration::from_millis(1500)),
        FarTurn::Blackholed,
    )
    .await?;
    println!("AC23 blackholed: {o}");
    assert!(o.remote.starts_with(&format!("{VIEWER_TUN_IP}:")), "on the mesh: {o}");
    assert!(o.after < Duration::from_secs(5), "AC23: {o}");
    Ok(())
}

/// The route rank's one predicate, on the field's candidates: a pair rides the
/// mesh when either end is on it, and the only pair it never forms is a local
/// off the mesh with a remote on it.
#[test]
fn a_pair_rides_the_mesh_when_either_end_is_on_it() {
    let route = route_rank(Duration::from_secs(3));
    let rank = mesh_last();
    let r = Some(&route);
    let lan_host = host(AGENT_LAN_IP, 1, host_priority(r, &ip(AGENT_LAN_IP), 1));
    let mesh_host = host(AGENT_MESH_IP, 2, host_priority(r, &ip(AGENT_MESH_IP), 1));
    let far_relay = relay("203.0.113.7", 3, relay_priority(r, &ip("203.0.113.7"), "turn.example.org", true, 1));
    // A relay the agent allocated on the viewer's own relay: served from the mesh.
    let served = relay(VIEWER_TUN_IP, 4, relay_priority(r, &ip(VIEWER_TUN_IP), VIEWER_TUN_IP, false, 1));
    let viewer_relay = relay(VIEWER_TUN_IP, 5, chrome(2, 32542));
    let viewer_far = relay("198.51.100.9", 6, chrome(2, 32542));
    for (name, local, remote, rides_mesh, crosses) in [
        ("shape A", &lan_host, &viewer_relay, true, true),
        ("shape B", &mesh_host, &viewer_relay, true, false),
        ("far TURN", &far_relay, &viewer_far, false, false),
        ("far relay to the mesh", &far_relay, &viewer_relay, true, true),
        ("served from the mesh", &served, &viewer_relay, true, false),
        ("LAN host to the far relay", &lan_host, &viewer_far, false, false),
    ] {
        assert_eq!(route.mesh_route(&**local, &**remote), rides_mesh, "{name}");
        assert_eq!(route.crosses_into_the_mesh(&**local, &**remote), crosses, "{name}");
        assert!(!rank.crosses_into_the_mesh(&**local, &**remote), "{name}: prio forms every pair");
    }
}

// FR-148 P4 step 2, phase A of D49: R1, the hold's exits, A2, and the kill
// switch.

/// The viewer's own address on a LAN the agent shares (the one-LAN cell).
const VIEWER_LAN_IP: &str = "192.168.1.20";

/// A TURN URL as the agent's ICE servers carry it, at `host`.
fn turn_url(host: &str) -> Url {
    Url {
        scheme: SchemeType::Turn,
        host: host.to_owned(),
        port: VNET_STUN_SERVER_PORT,
        username: "user".to_owned(),
        password: "pass".to_owned(),
        proto: ProtoType::Udp,
    }
}

/// The candidates of the hold's unit cells: the agent's LAN host, its mesh
/// host (ranked last), the viewer's TUN host on the mesh, and the viewer's far
/// relay.
fn hold_cell_candidates() -> (Cand, Cand, Cand, Cand) {
    (
        host(AGENT_LAN_IP, 5000, 0),
        host(AGENT_MESH_IP, 5001, 255),
        host(VIEWER_TUN_IP, 7000, chrome(126, 65535)),
        relay("198.51.100.9", 7001, chrome(2, 32542)),
    )
}

/// An agent running `rank` (controlled unless `controlling`), with one TURN URL
/// off the mesh whose relay gather has ended without a relay, and its checks
/// started now. It gathers relays only, so this machine's own interfaces stay
/// out of the cell.
async fn agent_past_its_relay_gather(rank: Option<CarrierRank>, controlling: bool) -> Result<Agent> {
    let a = Agent::new(AgentConfig {
        urls: vec![turn_url("turn.example.org")],
        candidate_types: vec![CandidateType::Relay],
        is_controlling: controlling,
        carrier_rank: Arc::new(rank),
        ..Default::default()
    })
    .await?;
    Agent::gather_candidates_relay(vec![], Arc::new(net::Net::new(None)), Arc::clone(&a.internal)).await;
    assert_eq!(a.internal.real_turns_pending.load(Ordering::SeqCst), 0, "the relay gather ended");
    a.internal.route_hold.lock().started = Some(Instant::now());
    Ok(a)
}

/// R1 and the hold read the config before anything is gathered: a TURN URL on
/// the mesh is a sibling, one off it is an allocation the hold waits for, a
/// STUN URL is neither, and nothing counts without relay gathering, on a direct
/// link, or outside the route rank.
#[tokio::test]
async fn the_config_names_the_turn_sibling_and_the_allocations_the_hold_waits_for() -> Result<()> {
    let urls = || {
        vec![
            turn_url(VIEWER_TUN_IP),
            turn_url("turn.example.org"),
            turn_url("203.0.113.7"),
            Url {
                scheme: SchemeType::Stun,
                ..turn_url(MESH_SERVER_IP)
            },
        ]
    };
    let direct = || CarrierRank {
        mesh_last: None,
        ..route_rank(ROUTE_HOLD)
    };
    for (name, rank, types, sibling, pending) in [
        ("route", Some(route_rank(ROUTE_HOLD)), vec![CandidateType::Relay], SIBLING_TURN, 2),
        (
            "route, no relay gathering",
            Some(route_rank(ROUTE_HOLD)),
            vec![CandidateType::ServerReflexive],
            0,
            0,
        ),
        ("route, direct link", Some(direct()), vec![CandidateType::Relay], 0, 0),
        ("prio and 1", Some(mesh_last()), vec![CandidateType::Relay], 0, 0),
        ("0", None, vec![CandidateType::Relay], 0, 0),
    ] {
        let a = Agent::new(AgentConfig {
            urls: urls(),
            candidate_types: types,
            carrier_rank: Arc::new(rank),
            ..Default::default()
        })
        .await?;
        assert_eq!(a.internal.mesh_sibling.load(Ordering::SeqCst), sibling, "{name}");
        assert_eq!(a.internal.real_turns_pending.load(Ordering::SeqCst), pending, "{name}");
        a.close().await?;
    }
    Ok(())
}

/// R1 is decided before any candidate exists: whether the agent pairs its LAN
/// host with a remote on the mesh follows the session's sibling (here a host on
/// the mesh, read from the interfaces the host gather lists), never the order
/// the two candidates come in.
#[tokio::test]
async fn r1_the_lan_hosts_pair_into_the_mesh_follows_the_sibling_not_the_order() -> Result<()> {
    let no_sibling: &[&str] = &[AGENT_LAN_IP];
    let a_host_on_the_mesh: &[&str] = &[AGENT_LAN_IP, AGENT_MESH_IP];
    for (agent_ips, sibling) in [(no_sibling, 0), (a_host_on_the_mesh, SIBLING_HOST)] {
        for remote_first in [true, false] {
            let router = Arc::new(Mutex::new(router::Router::new(router::RouterConfig {
                cidr: "0.0.0.0/0".to_owned(),
                ..Default::default()
            })?));
            let agent_net = Arc::new(net::Net::new(Some(net::NetConfig {
                static_ips: agent_ips.iter().map(|s| (*s).to_owned()).collect(),
                ..Default::default()
            })));
            connect_net2router(&agent_net, &router).await?;
            let a = Agent::new(AgentConfig {
                network_types: vec![NetworkType::Udp4],
                multicast_dns_mode: MulticastDnsMode::Disabled,
                net: Some(agent_net),
                carrier_rank: Arc::new(Some(route_rank(ROUTE_HOLD))),
                ..Default::default()
            })
            .await?;
            let case = format!("agent at {agent_ips:?}, remote first: {remote_first}");
            assert_eq!(a.internal.mesh_sibling.load(Ordering::SeqCst), sibling, "{case}");
            let lan_host = host(AGENT_LAN_IP, 5000, 0);
            let viewer_relay = relay(VIEWER_TUN_IP, 7000, chrome(2, 32542));
            if remote_first {
                a.internal.add_remote_candidate(&viewer_relay).await;
                a.internal.add_candidate(&lan_host).await?;
            } else {
                a.internal.add_candidate(&lan_host).await?;
                a.internal.add_remote_candidate(&viewer_relay).await;
            }
            assert_eq!(
                a.internal.find_pair(&lan_host, &viewer_relay).await.is_some(),
                sibling == 0,
                "formed only without a sibling: {case}"
            );
            a.close().await?;
        }
    }
    Ok(())
}

/// R1's sibling is dropped when the gather that was to make it ends without it
/// (the TURN server on the mesh never answered), and the pairs into the mesh it
/// had refused are formed then. A relay that server did allocate keeps it.
#[tokio::test]
async fn r1_a_sibling_its_gather_never_made_is_dropped_and_the_refused_pair_formed() -> Result<()> {
    for made_it in [false, true] {
        let a = Agent::new(AgentConfig {
            urls: vec![turn_url(VIEWER_TUN_IP)],
            candidate_types: vec![CandidateType::Relay],
            carrier_rank: Arc::new(Some(route_rank(ROUTE_HOLD))),
            ..Default::default()
        })
        .await?;
        assert_eq!(a.internal.mesh_sibling.load(Ordering::SeqCst), SIBLING_TURN);
        let lan_host = host(AGENT_LAN_IP, 5000, 0);
        let viewer_relay = relay(VIEWER_TUN_IP, 7000, chrome(2, 32542));
        a.internal.add_candidate(&lan_host).await?;
        a.internal.add_remote_candidate(&viewer_relay).await;
        assert!(
            a.internal.find_pair(&lan_host, &viewer_relay).await.is_none(),
            "refused while the sibling may come"
        );
        if made_it {
            // The relay the TURN server on the mesh allocated: ranked last.
            a.internal.add_candidate(&relay("203.0.113.7", 6000, 255)).await?;
        }
        // The host gather's end does not touch a sibling it was not to make.
        a.internal.settle_sibling(SIBLING_HOST).await;
        assert_eq!(a.internal.mesh_sibling.load(Ordering::SeqCst), SIBLING_TURN);
        a.internal.settle_sibling(SIBLING_TURN).await;
        let left = a.internal.mesh_sibling.load(Ordering::SeqCst);
        assert_eq!(left, if made_it { SIBLING_TURN } else { 0 }, "made it: {made_it}");
        assert_eq!(
            a.internal.find_pair(&lan_host, &viewer_relay).await.is_some(),
            !made_it,
            "made it: {made_it}"
        );
        a.close().await?;
    }
    Ok(())
}

/// The hold's first exit: its bound. It ends there for good, whatever is still
/// alive off the mesh, and reports the bound as the time held.
#[tokio::test]
async fn the_hold_stops_at_its_bound_for_good() -> Result<()> {
    let bound = Duration::from_millis(300);
    let a = agent_past_its_relay_gather(Some(route_rank(bound)), false).await?;
    let (lan, mesh, tun, far) = hold_cell_candidates();
    a.internal.add_pair(Arc::clone(&lan), Arc::clone(&far)).await;
    a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&tun)).await;
    assert!(a.internal.holds_mesh_route().await, "a pair off the mesh is alive");
    assert_eq!(a.internal.route_hold_so_far(), (Duration::ZERO, None), "holding");
    tokio::time::sleep(bound + Duration::from_millis(50)).await;
    assert!(!a.internal.holds_mesh_route().await, "the bound");
    assert_eq!(
        a.internal.route_hold_so_far(),
        (bound, Some(RouteHoldExit::Bound))
    );
    // A pair off the mesh formed after the bound does not hold it again.
    a.internal
        .add_pair(Arc::clone(&lan), relay("198.51.100.10", 7002, chrome(2, 32542)))
        .await;
    assert!(!a.internal.holds_mesh_route().await, "over for good");
    a.close().await?;
    Ok(())
}

/// The hold's second exit: a pair off the mesh is selected. It ends there for
/// good, and reports the time to that selection.
#[tokio::test]
async fn the_hold_stops_when_a_pair_off_the_mesh_is_selected() -> Result<()> {
    let a = agent_past_its_relay_gather(Some(route_rank(Duration::from_secs(30))), false).await?;
    let (lan, mesh, tun, far) = hold_cell_candidates();
    a.internal.add_pair(Arc::clone(&lan), Arc::clone(&far)).await;
    a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&tun)).await;
    assert!(a.internal.holds_mesh_route().await);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let real = a.internal.find_pair(&lan, &far).await.expect("the pair off the mesh");
    a.internal.set_selected_pair(Some(real)).await;
    assert!(!a.internal.holds_mesh_route().await, "a real pair selected");
    let (held, exit) = a.internal.route_hold_so_far();
    assert_eq!(exit, Some(RouteHoldExit::RealSelected));
    assert!(held >= Duration::from_millis(50) && held < Duration::from_secs(5), "{held:?}");
    a.close().await?;
    Ok(())
}

/// The hold's third exit, and the browser's far relay arriving after the
/// agent's own relay gather has ended. With only mesh-route pairs the hold
/// stops: nothing real is left. The far relay's pair off the mesh holds it
/// again, until that pair has failed, and the mesh-route pairs are never
/// checked meanwhile.
#[tokio::test]
async fn a_far_relay_after_the_agents_relay_gather_holds_until_its_pair_fails() -> Result<()> {
    let a = agent_past_its_relay_gather(Some(route_rank(Duration::from_secs(30))), false).await?;
    let (lan, mesh, tun, far) = hold_cell_candidates();
    // R1: no sibling here, so the LAN host's pair into the mesh is formed.
    a.internal.add_pair(Arc::clone(&lan), Arc::clone(&tun)).await;
    a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&tun)).await;
    assert!(!a.internal.holds_mesh_route().await, "nothing off the mesh can come");
    assert_eq!(a.internal.route_hold_so_far().1, Some(RouteHoldExit::NothingReal));

    // The far relay, late.
    a.internal.add_pair(Arc::clone(&lan), Arc::clone(&far)).await;
    a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&far)).await;
    assert!(a.internal.holds_mesh_route().await, "a pair off the mesh can still validate");
    assert_eq!(a.internal.route_hold_so_far(), (Duration::ZERO, None), "holding again");

    let (waiting, in_progress, failed) = (
        CandidatePairState::Waiting as u8,
        CandidatePairState::InProgress as u8,
        CandidatePairState::Failed as u8,
    );
    // Nobody answers: the pair off the mesh fails after its bounded checks.
    for _ in 0..9 {
        a.internal.contact_candidates().await;
    }
    assert_eq!(checks_of(&a, &lan, &far).await, (failed, 8));
    for (local, remote) in [(&lan, &tun), (&mesh, &tun), (&mesh, &far)] {
        assert_eq!(
            checks_of(&a, local, remote).await,
            (waiting, 0),
            "held, never checked: {local} -> {remote}"
        );
    }
    assert!(!a.internal.holds_mesh_route().await, "nothing real left");
    let (held, exit) = a.internal.route_hold_so_far();
    assert_eq!(exit, Some(RouteHoldExit::NothingReal));
    assert!(held > Duration::ZERO, "{held:?}");
    // Stopped, the mesh-route pairs are checked.
    a.internal.contact_candidates().await;
    assert_eq!(checks_of(&a, &mesh, &tun).await, (in_progress, 1));
    assert_eq!(checks_of(&a, &lan, &tun).await, (in_progress, 1));
    a.close().await?;
    Ok(())
}

/// A selection ends the hold for good, one on the mesh too: here the hold
/// stops at `nothing-real`, the pair on the mesh is selected, and only then
/// does the browser's far relay arrive (A5's case, trickled late). Its pairs do
/// not hold the mesh again, so the pair that carries the session is still
/// checked and answered, and the read at the opening keeps the hold's stop.
#[tokio::test]
async fn after_a_selection_on_the_mesh_a_late_far_relay_never_holds_it_again() -> Result<()> {
    let a = agent_past_its_relay_gather(Some(route_rank(Duration::from_secs(30))), false).await?;
    let (lan, mesh, tun, far) = hold_cell_candidates();
    a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&tun)).await;
    assert!(!a.internal.holds_mesh_route().await, "nothing off the mesh can come");
    let on_the_mesh = a.internal.find_pair(&mesh, &tun).await.expect("the pair on the mesh");
    a.internal.set_selected_pair(Some(on_the_mesh)).await;
    let stopped = a.internal.route_hold_so_far();
    assert_eq!(stopped.1, Some(RouteHoldExit::NothingReal));

    // The far relay, after the selection.
    a.internal.add_pair(Arc::clone(&lan), Arc::clone(&far)).await;
    a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&far)).await;
    assert!(!a.internal.holds_mesh_route().await, "a selection ends the hold for good");
    assert_eq!(a.internal.route_hold_so_far(), stopped, "the read at the opening stays");
    a.internal.ping_all_candidates().await;
    for (local, remote) in [(&mesh, &tun), (&mesh, &far), (&lan, &far)] {
        assert_eq!(
            checks_of(&a, local, remote).await,
            (CandidatePairState::InProgress as u8, 1),
            "checked, never held: {local} -> {remote}"
        );
    }
    a.close().await?;
    Ok(())
}

/// Where the hold never holds: the agent controls, it runs no route rank
/// (`prio` and `1`, or `0`), or the mesh link is direct. Nothing is held, every
/// pair is checked, no hold is reported, and only the route rank reads a pair.
#[tokio::test]
async fn the_hold_never_holds_for_the_controlling_side_without_route_or_on_a_direct_link() -> Result<()> {
    let direct = CarrierRank {
        mesh_last: None,
        ..route_rank(Duration::from_secs(30))
    };
    for (name, rank, controlling, reads) in [
        ("controlling", Some(route_rank(Duration::from_secs(30))), true, true),
        ("prio and 1", Some(mesh_last()), false, false),
        ("0", None, false, false),
        ("route, direct link", Some(direct), false, true),
    ] {
        let a = agent_past_its_relay_gather(rank, controlling).await?;
        let (lan, mesh, tun, far) = hold_cell_candidates();
        a.internal.add_pair(Arc::clone(&lan), Arc::clone(&far)).await;
        a.internal.add_pair(Arc::clone(&mesh), Arc::clone(&tun)).await;
        a.internal.add_pair(Arc::clone(&lan), Arc::clone(&tun)).await;
        assert!(!a.internal.holds_mesh_route().await, "{name}");
        if !controlling {
            a.internal.contact_candidates().await;
            for (local, remote) in [(&lan, &far), (&mesh, &tun), (&lan, &tun)] {
                assert_eq!(
                    checks_of(&a, local, remote).await,
                    (CandidatePairState::InProgress as u8, 1),
                    "{name}: checked at once: {local} -> {remote}"
                );
            }
        }
        let p = a.internal.find_pair(&mesh, &tun).await.unwrap();
        a.internal.set_selected_pair(Some(p)).await;
        assert_eq!(a.internal.route_hold_so_far(), (Duration::ZERO, None), "{name}");
        let read = a.route_read(mesh.priority(), mesh.component(), &mesh.address(), &tun.address());
        assert_eq!(read.is_some(), reads, "{name}");
        if let Some(read) = read {
            assert_eq!(
                read,
                RouteRead {
                    rides_the_mesh: true,
                    held: Duration::ZERO,
                    hold_exit: None
                },
                "{name}"
            );
        }
        a.close().await?;
    }
    Ok(())
}

/// The kill switch: under `prio` and `1` (the rank without its route half)
/// every pair is formed, the pair into the mesh too, nothing is held, and the
/// agent reads no route.
#[tokio::test]
async fn kill_switch_under_prio_and_1_every_pair_is_formed_and_nothing_is_held() -> Result<()> {
    let a = Agent::new(AgentConfig {
        urls: vec![turn_url(VIEWER_TUN_IP), turn_url("turn.example.org")],
        candidate_types: vec![CandidateType::Relay],
        carrier_rank: Arc::new(Some(mesh_last())),
        ..Default::default()
    })
    .await?;
    assert_eq!(a.internal.mesh_sibling.load(Ordering::SeqCst), 0);
    assert_eq!(a.internal.real_turns_pending.load(Ordering::SeqCst), 0);
    a.internal.route_hold.lock().started = Some(Instant::now());
    let (lan, mesh, tun, far) = hold_cell_candidates();
    a.internal.add_candidate(&lan).await?;
    a.internal.add_candidate(&mesh).await?;
    a.internal.add_remote_candidate(&tun).await;
    a.internal.add_remote_candidate(&far).await;
    let pairs = [(&lan, &tun), (&lan, &far), (&mesh, &tun), (&mesh, &far)];
    for (local, remote) in pairs {
        assert!(
            a.internal.find_pair(local, remote).await.is_some(),
            "formed: {local} -> {remote}"
        );
    }
    assert!(!a.internal.holds_mesh_route().await);
    a.internal.contact_candidates().await;
    for (local, remote) in pairs {
        assert_eq!(
            checks_of(&a, local, remote).await,
            (CandidatePairState::InProgress as u8, 1),
            "checked: {local} -> {remote}"
        );
    }
    assert!(a
        .route_read(lan.priority(), lan.component(), &lan.address(), &tun.address())
        .is_none());
    assert_eq!(a.internal.route_hold_so_far(), (Duration::ZERO, None));
    a.close().await?;
    Ok(())
}

/// The kill switch at the opening: under `prio` the field's AC22 failure stands
/// as it was measured on 2026-11-20. Shapes A and B both open on the mesh
/// although the far TURN allocates, and the agent reads no route.
#[tokio::test]
async fn kill_switch_under_prio_shapes_a_and_b_still_open_on_the_mesh() -> Result<()> {
    for (shape, agent_ip) in [("A", AGENT_LAN_IP), ("B", AGENT_MESH_IP)] {
        let o = opening_on_a_flat_net(&[agent_ip], mesh_last(), FarTurn::Up).await?;
        println!("prio, shape {shape}: {o}");
        assert!(
            o.remote.starts_with(&format!("{VIEWER_TUN_IP}:")),
            "shape {shape} under prio, unchanged: {o}"
        );
        assert!(o.read.is_none(), "shape {shape}: {o}");
    }
    Ok(())
}

/// A2: under the route rank the follow gate and the checks above the selection
/// read rule 1's predicate with the mesh's own blocks. An address of
/// 100.64.0.0/10 outside them (a carrier's CGNAT, a corporate VPN) is a real
/// path; a pair the mesh carries, by either end, is not, on a direct link too.
/// Under `prio` the gate is as built: the whole range is the overlay's.
#[tokio::test]
async fn a2_under_route_the_follow_gate_reads_the_mesh_blocks_not_the_whole_range() -> Result<()> {
    let route = route_rank(ROUTE_HOLD);
    let direct = CarrierRank {
        mesh_last: None,
        ..route_rank(ROUTE_HOLD)
    };
    let tun = host(VIEWER_TUN_IP, 2, chrome(126, 65535));
    // The current pair is alive: no stale-pair failover decides here.
    tun.seen(false);
    let selected = CandidatePair::new(host(AGENT_MESH_IP, 1, 255), Arc::clone(&tun), false);
    let turn = |remote: Cand| CandidatePair::new(relay("203.0.113.7", 3, UDP_RELAY), remote, false);

    // A remote in the /10 but outside the mesh's blocks.
    let cgnat = turn(srflx("100.80.1.2", 4, chrome(100, 65535)));
    assert!(outranks_off_the_mesh(&route, &cgnat, &selected));
    assert!(outranks_off_the_mesh(&direct, &cgnat, &selected));
    assert!(!outranks_on_a_real_path(&cgnat, &selected), "prio: the /10 is the overlay's");
    assert!(!should_follow_renomination(&cgnat, &selected));
    // A remote on the mesh: never, on a direct link either.
    let to_the_mesh = turn(relay(VIEWER_TUN_IP, 5, chrome(2, 65535)));
    // A local on the mesh with a real remote: today's gate reads the remote only.
    let from_the_mesh = CandidatePair::new(
        host(AGENT_MESH_IP, 6, TYPE_90),
        srflx("198.51.100.4", 7, chrome(100, 65535)),
        false,
    );
    assert!(outranks_on_a_real_path(&from_the_mesh, &selected));
    // A name stays overlay-possible.
    let name = turn(host("4a5b6c7d.local", 8, chrome(126, 65535)));
    for rank in [&route, &direct] {
        assert!(!outranks_off_the_mesh(rank, &to_the_mesh, &selected));
        assert!(!outranks_off_the_mesh(rank, &from_the_mesh, &selected));
        assert!(!outranks_off_the_mesh(rank, &name, &selected));
    }
    // Equal is not above.
    assert!(!outranks_off_the_mesh(&route, &cgnat, &cgnat));

    // The agent applies the route rank's gate under `route`, and today's
    // otherwise: the same renomination, followed or not.
    for (name, rank, follows) in [
        ("route", Some(route_rank(ROUTE_HOLD)), true),
        ("route, direct link", Some(CarrierRank { mesh_last: None, ..route_rank(ROUTE_HOLD) }), true),
        ("prio and 1", Some(mesh_last()), false),
        ("0", None, false),
    ] {
        let a = Agent::new(AgentConfig {
            candidate_types: vec![CandidateType::Relay],
            carrier_rank: Arc::new(rank),
            ..Default::default()
        })
        .await?;
        assert_eq!(a.internal.follows_renomination(&cgnat, &selected), follows, "{name}");
        assert_eq!(a.internal.outranks(&cgnat, &selected), follows, "{name}");
        assert!(!a.internal.follows_renomination(&to_the_mesh, &selected), "{name}");
        a.close().await?;
    }
    Ok(())
}

/// A2: the controlled agent's checks above its selection read the same
/// predicate. Under `route` a pair whose remote is in the /10 but off the
/// mesh's blocks is checked; under `prio` it is not, as built.
#[tokio::test]
async fn a2_the_checks_above_the_selection_read_the_same_predicate() -> Result<()> {
    for (name, rank, checks_cgnat) in [
        ("route", route_rank(ROUTE_HOLD), true),
        ("prio and 1", mesh_last(), false),
    ] {
        let a = Agent::new(AgentConfig {
            candidate_types: vec![CandidateType::Relay],
            carrier_rank: Arc::new(Some(rank)),
            ..Default::default()
        })
        .await?;
        a.internal.route_hold.lock().started = Some(Instant::now());
        let mesh_host = host(AGENT_MESH_IP, 5000, 255);
        let tun = host(VIEWER_TUN_IP, 7000, chrome(126, 65535));
        tun.seen(false);
        a.internal
            .set_selected_pair(Some(Arc::new(CandidatePair::new(
                Arc::clone(&mesh_host),
                Arc::clone(&tun),
                false,
            ))))
            .await;
        let turn = relay("203.0.113.7", 6000, UDP_RELAY);
        let cgnat = srflx("100.80.1.2", 7001, chrome(100, 65535));
        let real = srflx("198.51.100.4", 7002, chrome(100, 65535));
        a.internal.add_pair(Arc::clone(&turn), Arc::clone(&cgnat)).await;
        a.internal.add_pair(Arc::clone(&turn), Arc::clone(&real)).await;
        a.internal.add_pair(Arc::clone(&turn), Arc::clone(&tun)).await;
        a.internal.contact_candidates().await;
        let (waiting, in_progress) = (
            CandidatePairState::Waiting as u8,
            CandidatePairState::InProgress as u8,
        );
        assert_eq!(checks_of(&a, &turn, &real).await, (in_progress, 1), "{name}");
        assert_eq!(
            checks_of(&a, &turn, &cgnat).await,
            if checks_cgnat { (in_progress, 1) } else { (waiting, 0) },
            "{name}"
        );
        assert_eq!(checks_of(&a, &turn, &tun).await, (waiting, 0), "{name}: a mesh remote");
        a.close().await?;
    }
    Ok(())
}

/// One LAN: both ends on one LAN and on the mesh. The native pair is off the
/// mesh, so nothing holds it: the opening is the native pair, as fast as under
/// `prio`, far inside the product's bound.
#[tokio::test]
async fn one_lan_the_native_pair_opens_with_no_hold_delay() -> Result<()> {
    let flat = || FlatNet {
        agent_ips: &[AGENT_LAN_IP, AGENT_MESH_IP],
        browser_ips: &[VIEWER_LAN_IP, VIEWER_TUN_IP],
        far_turn: FarTurn::Up,
        browser_trickles: true,
    };
    let prio = opening(flat(), mesh_last()).await?;
    let route = opening(flat(), route_rank(ROUTE_HOLD)).await?;
    println!("one LAN, prio: {prio}");
    println!("one LAN, route: {route}");
    assert!(
        route.local.starts_with(&format!("{AGENT_LAN_IP}:"))
            && route.remote.starts_with(&format!("{VIEWER_LAN_IP}:")),
        "the native pair: {route}"
    );
    assert_eq!(
        route.read.and_then(|r| r.hold_exit),
        Some(RouteHoldExit::RealSelected),
        "{route}"
    );
    assert!(
        route.after < prio.after + Duration::from_millis(500),
        "as fast as under prio: {route} / {prio}"
    );
    assert!(route.after < Duration::from_secs(1), "no hold delay: {route}");
    Ok(())
}

/// R1, no sibling: the agent has no host on the mesh (a Linux agent whose
/// overlay interface the name filter drops) and no TURN URL on it, and the far
/// TURN is blackholed. Its LAN host's pair into the mesh is the route's only
/// pair: formed, held to the product's bound, then answered, and the session is
/// on the mesh within AC23's 5 s. With a host on the mesh the same pair is
/// never formed, and the session opens on the sibling's own pair.
#[tokio::test]
async fn r1_without_a_sibling_the_pair_into_the_mesh_is_formed_held_then_answered() -> Result<()> {
    let o = opening_on_a_flat_net(&[AGENT_LAN_IP], route_rank(ROUTE_HOLD), FarTurn::Blackholed).await?;
    println!("R1, no sibling: {o}");
    assert!(
        o.local.starts_with(&format!("{AGENT_LAN_IP}:"))
            && o.remote.starts_with(&format!("{VIEWER_TUN_IP}:")),
        "the pair into the mesh: {o}"
    );
    assert!(o.after >= ROUTE_HOLD, "held to the bound: {o}");
    assert!(o.after < Duration::from_secs(5), "AC23: {o}");
    assert_eq!(
        o.read,
        Some(RouteRead {
            rides_the_mesh: true,
            held: ROUTE_HOLD,
            hold_exit: Some(RouteHoldExit::Bound)
        }),
        "{o}"
    );

    let o = opening_on_a_flat_net(
        &[AGENT_LAN_IP, AGENT_MESH_IP],
        route_rank(ROUTE_HOLD),
        FarTurn::Blackholed,
    )
    .await?;
    println!("R1, a host on the mesh: {o}");
    assert!(!o.formed(AGENT_LAN_IP, VIEWER_TUN_IP), "refused: {o}");
    assert!(o.local.starts_with(&format!("{AGENT_MESH_IP}:")), "the sibling's pair: {o}");
    assert!(o.after < Duration::from_secs(5), "AC23: {o}");
    Ok(())
}

/// A peer-reflexive remote on the mesh: the browser's candidates never reach
/// the agent, which learns the browser's mesh address from its checks alone.
/// With a sibling (a host on the mesh) the LAN host's pair to it is refused,
/// and the browser's checks there go unanswered for good; without one it is
/// formed, held to the bound, then answered.
#[tokio::test]
async fn a_peer_reflexive_remote_on_the_mesh_is_refused_with_a_sibling_and_held_without() -> Result<()> {
    let hold = Duration::from_secs(1);
    let flat = |agent_ips: &'static [&'static str]| FlatNet {
        agent_ips,
        browser_ips: &[VIEWER_TUN_IP],
        far_turn: FarTurn::Blackholed,
        browser_trickles: false,
    };
    let o = opening(flat(&[AGENT_LAN_IP, AGENT_MESH_IP]), route_rank(hold)).await?;
    println!("prflx, a host on the mesh: {o}");
    assert_eq!(o.remote_typ, CandidateType::PeerReflexive, "{o}");
    assert!(!o.formed(AGENT_LAN_IP, VIEWER_TUN_IP), "refused: {o}");
    assert!(o.local.starts_with(&format!("{AGENT_MESH_IP}:")), "{o}");
    assert!(o.after >= hold, "held: {o}");

    let o = opening(flat(&[AGENT_LAN_IP]), route_rank(hold)).await?;
    println!("prflx, no sibling: {o}");
    assert_eq!(o.remote_typ, CandidateType::PeerReflexive, "{o}");
    assert!(
        o.local.starts_with(&format!("{AGENT_LAN_IP}:"))
            && o.remote.starts_with(&format!("{VIEWER_TUN_IP}:")),
        "formed: {o}"
    );
    assert!(o.after >= hold, "held: {o}");
    assert_eq!(o.read.and_then(|r| r.hold_exit), Some(RouteHoldExit::Bound), "{o}");
    Ok(())
}

/// AC23 at the product's bound (`ROUTE_HOLD`, 2.5 s), not only at a test's:
/// the far TURN blackholed, the session is on the mesh within 5 s.
#[tokio::test]
async fn ac23_at_the_products_hold_a_blackholed_turn_opens_on_the_mesh_within_5_s() -> Result<()> {
    let o = opening_on_a_flat_net(&[AGENT_MESH_IP], route_rank(ROUTE_HOLD), FarTurn::Blackholed).await?;
    println!("AC23 at ROUTE_HOLD: {o}");
    assert!(o.remote.starts_with(&format!("{VIEWER_TUN_IP}:")), "on the mesh: {o}");
    assert!(o.after < Duration::from_secs(5), "AC23: {o}");
    assert_eq!(o.read.and_then(|r| r.hold_exit), Some(RouteHoldExit::Bound), "{o}");
    Ok(())
}

// FR-148 P4 step 2, phase B of D49: the move after the selection, under the
// route rank. B1: a pair above a selection on the mesh is checked on a budget
// of its own. B2: a nomination of a pair the agent has not validated is
// answered, with a triggered check, and taken once that check succeeds; the
// agent never sends on a pair it has not validated. Under `prio`, `1` and `0`
// none of it acts.

/// B1's budget, as the cells read it: the product's.
const MOVE_BUDGET: u16 = ROUTE_MOVE_CHECKS;

/// B1's budget in the product: 13 checks at the loop's 200 ms span 2.4 s, the
/// hold's own bound less one check interval.
#[test]
fn b1_the_budget_spans_the_holds_bound_at_the_check_cadence() {
    assert_eq!(ROUTE_MOVE_CHECKS, 13);
    let span = DEFAULT_CHECK_INTERVAL * u32::from(ROUTE_MOVE_CHECKS - 1);
    assert_eq!(span, Duration::from_millis(2400));
    assert!(span < ROUTE_HOLD && ROUTE_HOLD - span <= DEFAULT_CHECK_INTERVAL);
}

/// B1: while the selection rides the mesh, a pair off the mesh above it is
/// checked on a budget of its own, then it fails and is never checked again.
/// The opening's checks do not spend it: a pair whose opening budget is gone
/// gets the whole of it. With the selection off the mesh, under `prio` and `1`,
/// and under `0`, the checks are phase A's as built: `max_binding_requests + 1`
/// of them, or none.
#[tokio::test]
async fn b1_a_pair_above_a_selection_on_the_mesh_has_a_budget_of_its_own() -> Result<()> {
    let (waiting, in_progress, failed) = (
        CandidatePairState::Waiting as u8,
        CandidatePairState::InProgress as u8,
        CandidatePairState::Failed as u8,
    );
    for (name, rank, on_the_mesh) in [
        ("route, the selection on the mesh", Some(route_rank(ROUTE_HOLD)), true),
        ("route, the selection off the mesh", Some(route_rank(ROUTE_HOLD)), false),
        ("prio and 1", Some(mesh_last()), true),
        ("0", None, true),
    ] {
        let moves = on_the_mesh && rank.as_ref().is_some_and(|r| r.route.is_some());
        let checks = rank.is_some();
        let a = Agent::new(AgentConfig {
            candidate_types: vec![CandidateType::Relay],
            carrier_rank: Arc::new(rank),
            ..Default::default()
        })
        .await?;
        a.internal.route_hold.lock().started = Some(Instant::now());
        let lan = host(AGENT_LAN_IP, 5000, chrome(126, 65535));
        let (local, remote) = if on_the_mesh {
            (host(AGENT_MESH_IP, 5001, 255), host(VIEWER_TUN_IP, 7000, chrome(126, 65535)))
        } else {
            (Arc::clone(&lan), relay("198.51.100.9", 7001, chrome(2, 32542)))
        };
        // The selection is alive: no stale-pair failover decides here.
        remote.seen(false);
        a.internal
            .set_selected_pair(Some(Arc::new(CandidatePair::new(local, remote, false))))
            .await;
        // Two native pairs above it: one formed after the selection, and one
        // the opening checked until its budget was gone.
        let fresh = srflx("198.51.100.4", 7002, chrome(100, 65535));
        let spent = srflx("198.51.100.5", 7003, chrome(100, 65535));
        a.internal.add_pair(Arc::clone(&lan), Arc::clone(&fresh)).await;
        a.internal.add_pair(Arc::clone(&lan), Arc::clone(&spent)).await;
        let opening_budget = a.internal.max_binding_requests + 1;
        {
            let p = a.internal.find_pair(&lan, &spent).await.unwrap();
            p.state.store(in_progress, Ordering::SeqCst);
            p.binding_request_count.store(opening_budget, Ordering::SeqCst);
        }

        for _ in 0..MOVE_BUDGET {
            a.internal.contact_candidates().await;
        }
        let (fresh_now, spent_now) = if moves {
            ((in_progress, MOVE_BUDGET), (in_progress, opening_budget + MOVE_BUDGET))
        } else if checks {
            ((failed, opening_budget), (failed, opening_budget))
        } else {
            ((waiting, 0), (in_progress, opening_budget))
        };
        assert_eq!(checks_of(&a, &lan, &fresh).await, fresh_now, "{name}: formed after the selection");
        assert_eq!(checks_of(&a, &lan, &spent).await, spent_now, "{name}: spent by the opening");

        // Past the budget: failed, and never checked again.
        for _ in 0..3 {
            a.internal.contact_candidates().await;
        }
        let end = |(state, count): (u8, u16)| if checks { (failed, count) } else { (state, count) };
        assert_eq!(checks_of(&a, &lan, &fresh).await, end(fresh_now), "{name}: formed after the selection");
        assert_eq!(checks_of(&a, &lan, &spent).await, end(spent_now), "{name}: spent by the opening");
        a.close().await?;
    }
    Ok(())
}

/// The STUN message type of a datagram that is one (RFC 5389: the magic cookie
/// at bytes 4 to 8).
fn stun_type(data: &[u8]) -> Option<u16> {
    (data.len() >= 20 && data[4..8] == [0x21, 0x12, 0xa4, 0x42]).then(|| u16::from_be_bytes([data[0], data[1]]))
}

/// What the agent's data channel writes every 20 ms once connected: a datagram
/// no STUN parser takes for one of its own.
const MEDIA: &[u8] = b"MEDIA";

/// What phase B's router lets through between the agent's and the viewer's
/// own LAN addresses, and what it saw there once that LAN opened; and whether
/// the agent's TURN allocation gets through.
#[derive(Default)]
struct Gate {
    /// The LAN passes, both ways.
    lan_open: std::sync::atomic::AtomicBool,
    /// The agent's own binding requests on the LAN are dropped; its answers
    /// pass: the half-blackholed pair.
    drops_agent_checks: std::sync::atomic::AtomicBool,
    /// The agent's requests to the far TURN's port are dropped.
    turn_dropped: std::sync::atomic::AtomicBool,
    /// What crossed the LAN, or was dropped there, once it opened, in order.
    seen: std::sync::Mutex<Vec<&'static str>>,
}

/// One flat net for phase B's cells, as [`opening`]'s: every hop 10 ms, every
/// address reaches every other, the LAN between the two ends' own addresses
/// behind the [`Gate`].
struct MoveNet<'a> {
    agent_ips: &'a [&'a str],
    browser_ips: &'a [&'a str],
    /// Both ends allocate on the far TURN.
    turn: bool,
    /// The agent's TURN allocation is dropped until this long after the
    /// checks start: a TURN that answers after the hold's bound.
    agent_turn_after: Option<Duration>,
}

/// A session [`phase_b_session`] opened, still running.
struct Session {
    router: Arc<Mutex<router::Router>>,
    server: Option<turn::server::Server>,
    gate: Arc<Gate>,
    agent: Arc<Agent>,
    browser: Arc<Agent>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _cancels: Vec<mpsc::Sender<()>>,
    started: Instant,
    /// The agent's first selection: its local and remote addresses, how long
    /// after the checks started, and the agent's route read of it.
    opened: (String, String),
    opened_after: Duration,
    read: Option<RouteRead>,
}

impl Session {
    async fn close(self) -> Result<()> {
        for t in &self.tasks {
            t.abort();
        }
        self.agent.close().await?;
        self.browser.close().await?;
        if let Some(server) = self.server {
            server.close().await?;
        }
        self.router.lock().await.stop().await?;
        Ok(())
    }
}

/// The agent's selection: its local and remote addresses and ports.
fn selection_of(agent: &Agent) -> Option<(String, u16, String, u16)> {
    agent
        .internal
        .agent_conn
        .get_selected_pair()
        .map(|p| (p.local.address(), p.local.port(), p.remote.address(), p.remote.port()))
}

/// A session on a [`MoveNet`], opened as [`opening`] opens one (the browser
/// selects and nominates as Chrome does), with the agent running `rank`. The
/// agent's data channel writes [`MEDIA`] once connected. Returns once the
/// agent has selected; the opening's Chrome stand-in stops there, so after it
/// a cell's own nominations are the browser's only ones.
async fn phase_b_session(net: MoveNet<'_>, rank: Option<CarrierRank>) -> Result<Session> {
    let router = Arc::new(Mutex::new(router::Router::new(router::RouterConfig {
        cidr: "0.0.0.0/0".to_owned(),
        min_delay: Duration::from_millis(10),
        ..Default::default()
    })?));
    let gate = Arc::new(Gate::default());
    gate.turn_dropped.store(net.agent_turn_after.is_some(), Ordering::SeqCst);
    {
        let gate = Arc::clone(&gate);
        let (agent_lan, viewer_lan, turn_ip) = (ip(AGENT_LAN_IP), ip(VIEWER_LAN_IP), ip(VNET_STUN_SERVER_IP));
        let agent_ips: Vec<IpAddr> = net.agent_ips.iter().map(|s| ip(s)).collect();
        router
            .lock()
            .await
            .add_chunk_filter(Box::new(move |c: &(dyn chunk::Chunk + Send + Sync)| {
                let (src, dst) = (c.get_source_ip(), c.get_destination_ip());
                if dst == turn_ip && c.destination_addr().port() == VNET_STUN_SERVER_PORT && agent_ips.contains(&src) {
                    return !gate.turn_dropped.load(Ordering::SeqCst);
                }
                let from_agent = src == agent_lan && dst == viewer_lan;
                if !from_agent && !(src == viewer_lan && dst == agent_lan) {
                    return true;
                }
                if !gate.lan_open.load(Ordering::SeqCst) {
                    return false;
                }
                let data = c.user_data();
                let (seen, passes) = match (from_agent, stun_type(&data)) {
                    (true, Some(0x0001)) if gate.drops_agent_checks.load(Ordering::SeqCst) => {
                        ("agent check dropped", false)
                    }
                    (true, Some(0x0001)) => ("agent check", true),
                    (true, Some(0x0101)) => ("agent answer", true),
                    (true, None) if data.starts_with(MEDIA) => ("agent media", true),
                    (false, Some(0x0001)) => ("browser check", true),
                    (false, Some(0x0101)) => ("answer to the agent", true),
                    _ => ("other", true),
                };
                gate.seen.lock().unwrap().push(seen);
                passes
            }))
            .await;
    }
    let viewer = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: net.browser_ips.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    })));
    let agent_net = Arc::new(net::Net::new(Some(net::NetConfig {
        static_ips: net.agent_ips.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    })));
    connect_net2router(&viewer, &router).await?;
    connect_net2router(&agent_net, &router).await?;
    let server = if net.turn {
        let turn_net = Arc::new(net::Net::new(Some(net::NetConfig {
            static_ip: VNET_STUN_SERVER_IP.to_owned(),
            ..Default::default()
        })));
        connect_net2router(&turn_net, &router).await?;
        start_router(&router).await?;
        Some(add_vnet_stun(turn_net).await?)
    } else {
        start_router(&router).await?;
        None
    };
    let urls = || {
        if net.turn {
            vec![turn_url(VNET_STUN_SERVER_IP)]
        } else {
            vec![]
        }
    };
    let never = Some(Duration::from_secs(3600));
    let browser = Arc::new(
        Agent::new(AgentConfig {
            urls: urls(),
            network_types: vec![NetworkType::Udp4],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            net: Some(Arc::clone(&viewer)),
            host_acceptance_min_wait: never,
            srflx_acceptance_min_wait: never,
            prflx_acceptance_min_wait: never,
            relay_acceptance_min_wait: never,
            max_binding_requests: Some(100),
            ..Default::default()
        })
        .await?,
    );
    let agent = Arc::new(
        Agent::new(AgentConfig {
            urls: urls(),
            network_types: vec![NetworkType::Udp4],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            net: Some(Arc::clone(&agent_net)),
            carrier_rank: Arc::new(rank),
            ..Default::default()
        })
        .await?,
    );
    for (from, to) in [(&agent, &browser), (&browser, &agent)] {
        let to = Arc::clone(to);
        from.on_candidate(Box::new(move |c: Option<Cand>| {
            let to = Arc::clone(&to);
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(c) = unmarshal_candidate(&c.marshal()) {
                        let _ = to.add_remote_candidate(&(Arc::new(c) as Cand));
                    }
                }
            })
        }));
    }

    let (agent_ufrag, agent_pwd) = agent.get_local_user_credentials().await;
    let (browser_ufrag, browser_pwd) = browser.get_local_user_credentials().await;
    agent.gather_candidates()?;
    browser.gather_candidates()?;
    let started = Instant::now();
    let mut tasks = vec![];
    if let Some(after) = net.agent_turn_after {
        let gate = Arc::clone(&gate);
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(after).await;
            gate.turn_dropped.store(false, Ordering::SeqCst);
        }));
    }
    let (agent_cancel, agent_cancel_rx) = mpsc::channel(1);
    let (browser_cancel, browser_cancel_rx) = mpsc::channel(1);
    {
        let agent = Arc::clone(&agent);
        tasks.push(tokio::spawn(async move {
            if let Ok(conn) = agent.accept(agent_cancel_rx, browser_ufrag, browser_pwd).await {
                loop {
                    let _ = util::Conn::send(&*conn, MEDIA).await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }));
    }
    {
        let browser = Arc::clone(&browser);
        tasks.push(tokio::spawn(async move {
            let _ = browser.dial(browser_cancel_rx, agent_ufrag, agent_pwd).await;
        }));
    }
    // Chrome at the opening: the first pair its own check validates,
    // nominated at once and again every 100 ms, until the agent selects.
    let chrome = {
        let browser = Arc::clone(&browser);
        tokio::spawn(async move {
            let mut chosen: Option<(Cand, Cand)> = None;
            loop {
                if chosen.is_none() {
                    let checklist = browser.internal.agent_conn.checklist.lock().await;
                    chosen = checklist
                        .iter()
                        .find(|p| p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8)
                        .map(|p| (Arc::clone(&p.local), Arc::clone(&p.remote)));
                }
                if let Some((local, remote)) = &chosen {
                    nominate(&browser, local, remote).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        })
    };

    let mut opened = None;
    while started.elapsed() < Duration::from_secs(8) {
        if let Some(p) = agent.internal.agent_conn.get_selected_pair() {
            let read = agent.route_read(
                p.local.priority(),
                p.local.component(),
                &p.local.address(),
                &p.remote.address(),
            );
            opened = Some(((p.local.address(), p.remote.address()), started.elapsed(), read));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    chrome.abort();
    let session = Session {
        router,
        server,
        gate,
        agent,
        browser,
        tasks,
        _cancels: vec![agent_cancel, browser_cancel],
        started,
        opened: (String::new(), String::new()),
        opened_after: Duration::ZERO,
        read: None,
    };
    let Some((pair, after, read)) = opened else {
        session.close().await?;
        return Err(Error::Other("no selection within 8 s".to_owned()));
    };
    Ok(Session {
        opened: pair,
        opened_after: after,
        read,
        ..session
    })
}

/// A local candidate of `agent` at `address`, waited for up to 5 s.
async fn local_at(agent: &Agent, address: &str, typ: CandidateType) -> Result<Cand> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(c) = agent
            .get_local_candidates()
            .await?
            .into_iter()
            .find(|c| c.address() == address && c.candidate_type() == typ)
        {
            return Ok(c);
        }
        if Instant::now() >= until {
            return Err(Error::Other(format!("no {typ} candidate at {address}")));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A remote candidate of `agent` at `address` and `port`, waited for up to 5 s
/// (it comes by trickle).
async fn remote_soon(agent: &Agent, address: &str, port: u16) -> Result<Cand> {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(c) = remote_at(agent, address, port).await {
            return Ok(c);
        }
        if Instant::now() >= until {
            return Err(Error::Other(format!("no remote candidate at {address}:{port}")));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How one nomination of a pair the agent had not validated went
/// ([`one_nomination`]).
#[derive(Debug)]
struct Nominated {
    /// The agent's first selection: local and remote addresses.
    opened: (String, String),
    /// The browser's check was answered: its own pair stands validated.
    answered: bool,
    /// The agent's own check of the pair succeeded.
    validated: bool,
    /// The agent's selection is the pair.
    moved: bool,
    /// What crossed the LAN once it opened, in order.
    seen: Vec<&'static str>,
}

/// Phase B on the LAN: the session opens with the LAN between the two ends
/// shut, so its pair there is not the opening's. Once nothing checks that pair
/// any more (the agent failed it, or checks nothing after its selection), the
/// LAN opens, as a path that comes up after the opening, and the browser sends
/// ONE nomination of that pair (Chrome's USE-CANDIDATE) and nothing else on it.
/// `drops_agent_checks`: the agent's own checks on the LAN are dropped and its
/// answers pass, the half-blackholed pair. `turn`: both ends allocate on the far
/// TURN, so the session opens on a pair off the mesh under `route`.
async fn one_nomination(rank: Option<CarrierRank>, turn: bool, drops_agent_checks: bool) -> Result<Nominated> {
    let s = phase_b_session(
        MoveNet {
            agent_ips: &[AGENT_LAN_IP, AGENT_MESH_IP],
            browser_ips: &[VIEWER_LAN_IP, VIEWER_TUN_IP],
            turn,
            agent_turn_after: None,
        },
        rank,
    )
    .await?;
    let agent_lan = local_at(&s.agent, AGENT_LAN_IP, CandidateType::Host).await?;
    let viewer_lan = local_at(&s.browser, VIEWER_LAN_IP, CandidateType::Host).await?;
    let viewer_lan_at_agent = remote_soon(&s.agent, VIEWER_LAN_IP, viewer_lan.port()).await?;
    let agent_lan_at_browser = remote_soon(&s.browser, AGENT_LAN_IP, agent_lan.port()).await?;

    // Nothing checks the agent's LAN pair any more: it failed, or its count
    // held for three check intervals.
    let until = Instant::now() + Duration::from_secs(8);
    let (mut last, mut since) = (None, Instant::now());
    loop {
        let now = checks_of(&s.agent, &agent_lan, &viewer_lan_at_agent).await;
        if now.0 == CandidatePairState::Failed as u8 {
            break;
        }
        if last != Some(now) {
            (last, since) = (Some(now), Instant::now());
        } else if since.elapsed() >= Duration::from_millis(600) {
            break;
        }
        if Instant::now() >= until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    s.gate.drops_agent_checks.store(drops_agent_checks, Ordering::SeqCst);
    s.gate.lan_open.store(true, Ordering::SeqCst);
    nominate(&s.browser, &viewer_lan, &agent_lan_at_browser).await;
    let succeeded = Some(CandidatePairState::Succeeded as u8);
    let _ = within(Duration::from_secs(1), || async {
        pair_state(&s.agent, &agent_lan, &viewer_lan_at_agent).await == succeeded
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n = Nominated {
        opened: s.opened.clone(),
        answered: pair_state(&s.browser, &viewer_lan, &agent_lan_at_browser).await == succeeded,
        validated: pair_state(&s.agent, &agent_lan, &viewer_lan_at_agent).await == succeeded,
        moved: selection_of(&s.agent).is_some_and(|(l, _, r, _)| l == AGENT_LAN_IP && r == VIEWER_LAN_IP),
        seen: s.gate.seen.lock().unwrap().clone(),
    };
    s.close().await?;
    Ok(n)
}

/// B2: the session rides the mesh, and the browser nominates a pair off it that
/// the agent has not validated. The agent answers at once, sends its triggered
/// check, and takes the nomination when that check succeeds: one nomination
/// moves it. It sends nothing on the pair before its own check succeeded.
#[tokio::test]
async fn b2_a_nomination_the_agent_has_not_validated_is_answered_and_taken_on_its_own_check() -> Result<()> {
    let n = one_nomination(Some(route_rank(ROUTE_HOLD)), false, false).await?;
    println!("B2, one nomination: {n:?}");
    assert_eq!(n.opened.0, AGENT_MESH_IP, "the opening rides the mesh: {n:?}");
    assert!(n.answered, "answered at once: {n:?}");
    assert!(n.validated, "its triggered check: {n:?}");
    assert!(n.moved, "taken when its own check succeeded: {n:?}");
    let first = |what: &str| n.seen.iter().position(|e| *e == what);
    let (success, media) = (first("answer to the agent"), first("agent media"));
    assert!(success.is_some() && media.is_some(), "{n:?}");
    assert!(success < media, "nothing sent on the pair before its own check succeeded: {n:?}");
    Ok(())
}

/// B2, the half-blackholed pair (2026-07-27): the browser's checks reach the
/// agent on a pair off the mesh, but the agent's own checks are lost. The agent
/// answers, its triggered check fails, the selection stays on the mesh, and
/// nothing is sent on the pair it has not validated.
#[tokio::test]
async fn b2_the_half_blackholed_pair_is_answered_never_taken_and_never_sent_on() -> Result<()> {
    let n = one_nomination(Some(route_rank(ROUTE_HOLD)), false, true).await?;
    println!("B2, half-blackholed: {n:?}");
    assert_eq!(n.opened.0, AGENT_MESH_IP, "the opening rides the mesh: {n:?}");
    assert!(n.answered, "answered: {n:?}");
    assert!(n.seen.contains(&"agent check dropped"), "its triggered check was sent: {n:?}");
    assert!(!n.validated, "and never returned: {n:?}");
    assert!(!n.moved, "the selection stays on the mesh: {n:?}");
    assert!(!n.seen.contains(&"agent media"), "nothing sent on the pair: {n:?}");
    Ok(())
}

/// The kill switch: under `prio` and `1`, and under `0`, a nomination of a pair
/// the agent has not validated is handled as built: no answer, a triggered
/// check, and no move on one nomination.
#[tokio::test]
async fn b2_kill_switch_under_prio_and_0_a_nomination_the_agent_has_not_validated_is_handled_as_built(
) -> Result<()> {
    for (name, rank) in [("prio and 1", Some(mesh_last())), ("0", None)] {
        let n = one_nomination(rank, false, false).await?;
        println!("B2 kill switch, {name}: {n:?}");
        assert!(!n.answered, "{name}: no answer before its own check: {n:?}");
        assert!(!n.seen.contains(&"agent answer"), "{name}: {n:?}");
        assert!(n.validated, "{name}: the triggered check: {n:?}");
        assert!(!n.moved, "{name}: {n:?}");
    }
    Ok(())
}

/// No move away from a real pair: under `route`, with the selection off the
/// mesh (the far TURN carries the opening), B2 does not act. The nomination is
/// handled as built: no answer, a triggered check, no move.
#[tokio::test]
async fn b2_with_the_selection_off_the_mesh_a_nomination_is_handled_as_built() -> Result<()> {
    let n = one_nomination(Some(route_rank(ROUTE_HOLD)), true, false).await?;
    println!("B2, a real selection: {n:?}");
    assert!(
        n.opened.0 != AGENT_MESH_IP && n.opened.1 != VIEWER_TUN_IP,
        "the opening is off the mesh: {n:?}"
    );
    assert!(!n.answered, "no answer before its own check: {n:?}");
    assert!(!n.seen.contains(&"agent answer"), "{n:?}");
    assert!(n.validated, "the triggered check: {n:?}");
    assert!(!n.moved, "{n:?}");
    Ok(())
}

/// B1 on a live agent: once the session rides the mesh, a browser candidate
/// that answers nothing arrives. The agent checks its pair at the check
/// cadence, within the budget, then never again.
#[tokio::test]
async fn b1_a_dead_pair_above_a_selection_on_the_mesh_is_checked_at_the_check_cadence_within_the_budget(
) -> Result<()> {
    let s = phase_b_session(
        MoveNet {
            agent_ips: &[AGENT_LAN_IP, AGENT_MESH_IP],
            browser_ips: &[VIEWER_LAN_IP, VIEWER_TUN_IP],
            turn: false,
            agent_turn_after: None,
        },
        Some(route_rank(ROUTE_HOLD)),
    )
    .await?;
    assert_eq!(s.opened.0, AGENT_MESH_IP, "the opening rides the mesh");
    let agent_lan = local_at(&s.agent, AGENT_LAN_IP, CandidateType::Host).await?;
    s.agent
        .add_remote_candidate(&srflx("198.51.100.77", 9, chrome(100, 65535)))?;
    let dead = remote_soon(&s.agent, "198.51.100.77", 9).await?;
    let mut at = vec![];
    let mut last = 0;
    let watch = Instant::now();
    while watch.elapsed() < Duration::from_millis(4500) {
        if let Some(p) = s.agent.internal.find_pair(&agent_lan, &dead).await {
            let count = p.binding_request_count.load(Ordering::SeqCst);
            for _ in last..count {
                at.push(watch.elapsed());
            }
            last = count;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = checks_of(&s.agent, &agent_lan, &dead).await.0;
    s.close().await?;
    println!("B1 live: {} checks at {at:?}", at.len());
    assert_eq!(at.len(), MOVE_BUDGET as usize, "the budget: {at:?}");
    let gaps: Vec<_> = at.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.iter().all(|g| *g < Duration::from_millis(450)),
        "the check cadence, not the 2 s keepalive: {gaps:?}"
    );
    assert!(
        at.last().is_some_and(|last| *last < Duration::from_millis(3500)),
        "then never again: {at:?}"
    );
    assert_eq!(state, CandidatePairState::Failed as u8);
    Ok(())
}

/// When the agent's TURN allocation gets through in [`late_turn`]'s cells: 4 s
/// after the checks start, past `ROUTE_HOLD`.
const LATE: Duration = Duration::from_secs(4);

/// How a session whose agent relay allocated after the bound went
/// ([`late_turn`]).
#[derive(Debug)]
struct LateTurn {
    /// The agent's first selection: local and remote addresses.
    opened: (String, String),
    opened_after: Duration,
    read: Option<RouteRead>,
    /// From the checks' start to the agent's relay.
    relay_after: Duration,
    /// From the agent's relay to its selection of the two relays' pair.
    moved_after: Option<Duration>,
    /// The agent's selection changes after the opening.
    changes: usize,
    /// The agent's selection at the end is the two relays' pair.
    on_the_relays: bool,
}

impl LateTurn {
    /// The opening: on the mesh, at the hold's bound, read as such; then the
    /// relay, after the bound; then one move, to the relays' pair.
    fn opened_on_the_mesh_then_moved_once(&self) {
        assert_eq!(self.opened.0, AGENT_MESH_IP, "the opening rides the mesh: {self:?}");
        assert!(
            self.opened_after >= ROUTE_HOLD && self.opened_after < LATE,
            "at the bound: {self:?}"
        );
        assert_eq!(
            self.read.map(|r| (r.rides_the_mesh, r.hold_exit)),
            Some((true, Some(RouteHoldExit::Bound))),
            "hold_exit=bound: {self:?}"
        );
        assert!(self.relay_after > LATE, "the relay comes after the bound: {self:?}");
        assert!(self.on_the_relays, "on the relays' pair: {self:?}");
        assert_eq!(self.changes, 1, "the move is the only selection change: {self:?}");
    }
}

/// A TURN that answers after the hold's bound: the agent's allocation gets
/// through only at [`LATE`]. The session opens on the mesh at the bound; the
/// agent's relay comes later. The browser learns it by trickle at once, and
/// Chrome's stand-in checks the two relays' pair `first` after the agent's
/// relay, then every `every`: the pair ranks above Chrome's selection, so each
/// check nominates it (`SEMI_AGGRESSIVE`). The browser's TURN client asks for
/// its permission toward the agent's relayed address on its first check, so
/// the agent's own checks of the pair get through only after that. Watched for
/// 3.5 s after the agent's relay.
async fn late_turn(first: Duration, every: Duration) -> Result<LateTurn> {
    let s = phase_b_session(
        MoveNet {
            agent_ips: &[AGENT_MESH_IP],
            browser_ips: &[VIEWER_TUN_IP],
            turn: true,
            agent_turn_after: Some(LATE),
        },
        Some(route_rank(ROUTE_HOLD)),
    )
    .await?;
    let opening = selection_of(&s.agent);
    let relay = local_at(&s.agent, VNET_STUN_SERVER_IP, CandidateType::Relay).await;
    let relay_after = s.started.elapsed();
    let relayed = Instant::now();
    let browser_relay = local_at(&s.browser, VNET_STUN_SERVER_IP, CandidateType::Relay).await;
    let (relay, browser_relay) = match (relay, browser_relay) {
        (Ok(r), Ok(b)) => (r, b),
        (r, b) => {
            s.close().await?;
            return Err(Error::Other(format!("relays: agent {:?}, browser {:?}", r.err(), b.err())));
        }
    };
    let agent_relay_at_browser = remote_soon(&s.browser, &relay.address(), relay.port()).await?;
    let chrome = {
        let (browser, local, remote) = (Arc::clone(&s.browser), Arc::clone(&browser_relay), agent_relay_at_browser);
        tokio::spawn(async move {
            tokio::time::sleep(first).await;
            loop {
                nominate(&browser, &local, &remote).await;
                tokio::time::sleep(every).await;
            }
        })
    };
    let to = Some((relay.address(), relay.port(), browser_relay.address(), browser_relay.port()));
    let (mut changes, mut current, mut moved_after) = (0, opening, None);
    while relayed.elapsed() < Duration::from_millis(3500) {
        let now = selection_of(&s.agent);
        if now != current {
            changes += 1;
            current = now;
            if current == to && moved_after.is_none() {
                moved_after = Some(relayed.elapsed());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    chrome.abort();
    let t = LateTurn {
        opened: s.opened.clone(),
        opened_after: s.opened_after,
        read: s.read,
        relay_after,
        moved_after,
        changes,
        on_the_relays: current == to,
    };
    s.close().await?;
    Ok(t)
}

/// A late TURN, with a Chrome that checks the relays' pair at once and every
/// 480 ms (its strong ping interval, with no other connection to ping): the
/// session opens on the mesh (`hold_exit=bound`) and moves within 1.5 s of the
/// agent's relay, once.
#[tokio::test]
async fn b_a_turn_that_allocates_after_the_bound_opens_on_the_mesh_then_moves_once() -> Result<()> {
    let t = late_turn(Duration::ZERO, Duration::from_millis(480)).await?;
    println!("late TURN, Chrome every 480 ms: {t:?}");
    t.opened_on_the_mesh_then_moved_once();
    assert!(
        t.moved_after.is_some_and(|m| m < Duration::from_millis(1500)),
        "within 1.5 s of the relay: {t:?}"
    );
    Ok(())
}

/// A late TURN, with Chrome's round robin: its first check of the relays' pair
/// 300 ms after the agent's relay (the candidate's trip through the server and
/// Chrome's next ping slot), then every 2.4 s (one check per 480 ms over five
/// connections). Chrome's first check makes its TURN permission, so it reaches
/// the agent before any check of the agent's can be answered. Answered (B2), it
/// moves the session within 1 s of the agent's relay; unanswered, the move
/// waits for Chrome's second check, 2.7 s after the relay.
#[tokio::test]
async fn b2_a_late_turn_moves_on_chromes_first_check_of_the_pair() -> Result<()> {
    let t = late_turn(Duration::from_millis(300), Duration::from_millis(2400)).await?;
    println!("late TURN, Chrome's round robin: {t:?}");
    t.opened_on_the_mesh_then_moved_once();
    assert!(
        t.moved_after.is_some_and(|m| m < Duration::from_secs(1)),
        "on Chrome's first check, within 1 s of the relay: {t:?}"
    );
    Ok(())
}

/// B2's one decision (`AgentInternal::answers_before_validating`), which
/// answers a nomination before validating and takes it after: the route rank,
/// a selection that rides the mesh, a pair that does not, a remote that is an
/// address, and the follow gate. On a direct mesh link the rank's order holds:
/// a native pair is answered, a TURN pair below the direct mesh selection is
/// not. Nothing else answers before validating.
#[tokio::test]
async fn b2_one_decision_answers_only_a_pair_off_the_mesh_the_follow_gate_would_take() -> Result<()> {
    let direct = || CarrierRank {
        mesh_last: None,
        ..route_rank(ROUTE_HOLD)
    };
    let lan = || host(AGENT_LAN_IP, 5000, chrome(126, 65535));
    let native = || srflx("198.51.100.4", 7002, chrome(100, 65535));
    let viewer_tun = || host(VIEWER_TUN_IP, 7000, chrome(126, 65535));
    let viewer_far_relay = || relay("198.51.100.9", 7001, chrome(2, 32542));
    let on_relayed_mesh = || Some((host(AGENT_MESH_IP, 5001, 255), viewer_tun()));
    let on_direct_mesh = || Some((host(AGENT_MESH_IP, 5001, TYPE_90), viewer_tun()));
    let (fresh, stale) = (true, false);
    for (name, rank, selection, live, (local, remote), answers) in [
        ("route, on the mesh, a native pair", Some(route_rank(ROUTE_HOLD)), on_relayed_mesh(), fresh, (lan(), native()), true),
        ("route, on the mesh, a far TURN pair", Some(route_rank(ROUTE_HOLD)), on_relayed_mesh(), fresh, (lan(), viewer_far_relay()), true),
        ("route, a pair into the mesh", Some(route_rank(ROUTE_HOLD)), on_relayed_mesh(), fresh, (lan(), host(VIEWER_TUN_IP, 7003, chrome(126, 65535))), false),
        ("route, a pair from the mesh", Some(route_rank(ROUTE_HOLD)), on_relayed_mesh(), fresh, (host(AGENT_MESH_IP, 5009, 255), native()), false),
        ("route, the selection off the mesh", Some(route_rank(ROUTE_HOLD)), Some((lan(), viewer_far_relay())), fresh, (lan(), native()), false),
        ("route, nothing selected", Some(route_rank(ROUTE_HOLD)), None, fresh, (lan(), native()), false),
        ("route, a stale selection, a native pair", Some(route_rank(ROUTE_HOLD)), on_relayed_mesh(), stale, (lan(), native()), true),
        ("route, a stale selection, a name for the remote", Some(route_rank(ROUTE_HOLD)), on_relayed_mesh(), stale, (lan(), host("viewer.local", 7004, chrome(126, 65535))), false),
        ("direct link, a native pair", Some(direct()), on_direct_mesh(), fresh, (lan(), native()), true),
        ("direct link, a TURN pair below the direct mesh", Some(direct()), on_direct_mesh(), fresh, (lan(), viewer_far_relay()), false),
        ("prio and 1", Some(mesh_last()), on_relayed_mesh(), fresh, (lan(), native()), false),
        ("0", None, on_relayed_mesh(), fresh, (lan(), native()), false),
    ] {
        let a = Agent::new(AgentConfig {
            candidate_types: vec![CandidateType::Relay],
            carrier_rank: Arc::new(rank),
            ..Default::default()
        })
        .await?;
        a.internal.route_hold.lock().started = Some(Instant::now());
        if let Some((selected_local, selected_remote)) = selection {
            if live {
                selected_remote.seen(false);
            }
            a.internal
                .set_selected_pair(Some(Arc::new(CandidatePair::new(selected_local, selected_remote, false))))
                .await;
        }
        let pair = CandidatePair::new(local, remote, false);
        assert_eq!(a.internal.answers_before_validating(&pair), answers, "{name}");
        a.close().await?;
    }
    Ok(())
}
