use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use util::vnet::net::*;

use super::*;
use crate::error::*;
use crate::mdns::*;
use crate::network_type::*;
use crate::udp_network::UDPNetwork;
use crate::url::*;

/// The interval at which the agent performs candidate checks in the connecting phase.
pub(crate) const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_millis(200);

/// The interval used to keep candidates alive.
pub(crate) const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);

/// The default time till an Agent transitions disconnected.
pub(crate) const DEFAULT_DISCONNECTED_TIMEOUT: Duration = Duration::from_secs(5);

/// The default time till an Agent transitions to failed after disconnected.
pub(crate) const DEFAULT_FAILED_TIMEOUT: Duration = Duration::from_secs(25);

/// Wait time before nominating a host candidate.
pub(crate) const DEFAULT_HOST_ACCEPTANCE_MIN_WAIT: Duration = Duration::from_secs(0);

/// Wait time before nominating a srflx candidate.
pub(crate) const DEFAULT_SRFLX_ACCEPTANCE_MIN_WAIT: Duration = Duration::from_millis(500);

/// Wait time before nominating a prflx candidate.
pub(crate) const DEFAULT_PRFLX_ACCEPTANCE_MIN_WAIT: Duration = Duration::from_millis(1000);

/// Wait time before nominating a relay candidate.
pub(crate) const DEFAULT_RELAY_ACCEPTANCE_MIN_WAIT: Duration = Duration::from_millis(2000);

/// Max binding request before considering a pair failed.
pub(crate) const DEFAULT_MAX_BINDING_REQUESTS: u16 = 7;

/// The number of bytes that can be buffered before we start to error.
pub(crate) const MAX_BUFFER_SIZE: usize = 1000 * 1000; // 1MB

/// Wait time before binding requests can be deleted.
pub(crate) const MAX_BINDING_REQUEST_TIMEOUT: Duration = Duration::from_millis(4000);

pub(crate) fn default_candidate_types() -> Vec<CandidateType> {
    vec![
        CandidateType::Host,
        CandidateType::ServerReflexive,
        CandidateType::Relay,
    ]
}

pub type InterfaceFilterFn = Box<dyn (Fn(&str) -> bool) + Send + Sync>;
pub type IpFilterFn = Box<dyn (Fn(IpAddr) -> bool) + Send + Sync>;

/// Roomler patch (FR-148 P4): is this an address only the mesh carries? Asked
/// of a host candidate's interface address, a server-reflexive candidate's
/// mapped address, and a relay candidate's relayed address and TURN server.
pub type MeshAddrFn = Box<dyn (Fn(IpAddr) -> bool) + Send + Sync>;

/// Roomler patch (FR-148 P4): the carrier rank, one per agent. Absent, the
/// agent ranks and checks exactly as before. Present:
///
/// - a relay candidate allocated over TCP or TLS has local preference 32767,
///   below one allocated over UDP (65535);
/// - the controlled agent keeps checking after its first selection: every
///   pair the follow gate would follow, and nothing else
///   (`AgentInternal::outranks`: `agent_selector::outranks_on_a_real_path`,
///   or under the route rank `outranks_off_the_mesh`);
/// - with `mesh_last` set (the mesh link is not direct), every local candidate
///   on an address it names has type preference 0 and local preference 0
///   ([`ranked_last_priority`]). That is the candidate's own priority: its
///   pairs, the follow gate and its signalled line all read it. No other
///   candidate on either side can rank that low, so every pair it is in ranks
///   below every pair it is not in, for both roles.
#[derive(Default)]
pub struct CarrierRank {
    pub mesh_last: Option<MeshAddrFn>,
    /// Roomler patch (FR-148 P4 step 2, D49): the route rank. `None` keeps the
    /// rank above exactly as it was. Set, a pair's route is the rank's unit,
    /// not a candidate's address, and one predicate says which pairs ride the
    /// mesh ([`rides_the_mesh`], rule 1). The follow gate and the checks after
    /// the selection read it whatever the link, so they never take a pair the
    /// mesh carries for a real path. With `mesh_last` set too (the mesh link is
    /// not direct; [`CarrierRank::route_engaged`]), the route rank also acts:
    ///
    /// - rule 2: a local candidate off the mesh is not paired with a remote on
    ///   it while the session has a local candidate on the mesh or a TURN URL
    ///   on the mesh (the field's shape A: a LAN socket whose checks reach a
    ///   mesh address only because the destination picks the route). Such a
    ///   route is then also a pair of the agent's own mesh candidates, which
    ///   the rank ranks. A session with neither (R1) forms the pair and treats
    ///   it as any pair the mesh carries;
    /// - rule 3: the controlled agent holds its mesh-route pairs, neither
    ///   checking nor answering them, while a relay off the mesh can still come
    ///   and none carries the session yet, for [`RouteRank::hold`] at most (the
    ///   field's shape B: the mesh pair validates first, and the browser
    ///   selects the first pair it validates).
    pub route: Option<RouteRank>,
}

/// Roomler patch (FR-148 P4 step 2): see [`CarrierRank::route`].
pub struct RouteRank {
    /// How long after the checks start the hold ends, whatever is pending.
    pub hold: Duration,
    /// The mesh's own addresses (D12: its blocks and the derived-v6 ULA, never
    /// all of 100.64.0.0/10), whatever the mesh link. With `mesh_last` set they
    /// are the addresses it names.
    pub mesh: MeshAddrFn,
}

/// Roomler patch (FR-148 P4 step 2): the hold's bound in the product. AC23's
/// 5 s, less a mesh pair's check and nomination once the hold ends (a relayed
/// mesh round trip or two, under a second) and a margin.
pub const ROUTE_HOLD: Duration = Duration::from_millis(2500);

/// Roomler patch (FR-148 P4 step 2, phase B of D49, B1): the checks a pair off
/// the mesh gets above a selection that rides the mesh, on a budget of its own
/// (`CandidatePair::move_checks`), whatever the opening spent on it. They go at
/// the check cadence: the agent's loop ticks every [`DEFAULT_CHECK_INTERVAL`]
/// (200 ms) once connected too, since the keepalive interval only lowers it.
/// 13 checks span 2.4 s, inside [`ROUTE_HOLD`]: a pair that forms after the
/// selection gets the hold's own bound again for its far side to answer (the
/// candidate's signalling, the far TURN's permission for this agent's relayed
/// address). Then the pair fails and is not checked again: a dead pair costs 13
/// checks once. The far side's checks still reopen it: each is answered or
/// nominates (B2), and triggers one of this agent's.
pub const ROUTE_MOVE_CHECKS: u16 = 13;

/// Roomler patch (FR-148 P4 step 2): why the route rank's hold stopped holding,
/// the path line's `hold_exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteHoldExit {
    /// A pair off the mesh was selected.
    RealSelected,
    /// No relay off the mesh could still come: the relay gather to every TURN
    /// server off the mesh had ended, and every pair off the mesh had failed.
    NothingReal,
    /// [`RouteRank::hold`] passed since the checks started.
    Bound,
}

impl fmt::Display for RouteHoldExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::RealSelected => "real-selected",
            Self::NothingReal => "nothing-real",
            Self::Bound => "bound",
        })
    }
}

/// Roomler patch (FR-148 P4 step 2): a pair as the agent's route rank reads it,
/// with the hold so far: what a session's opening reads once
/// (`Agent::route_read`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteRead {
    /// Rule 1: does the pair ride the mesh ([`rides_the_mesh`])?
    pub rides_the_mesh: bool,
    /// From the checks' start to when the hold stopped holding; zero when it
    /// never stopped.
    pub held: Duration,
    /// Why it stopped; `None` when it never applied (the mesh link is direct,
    /// the agent controls) or has not stopped.
    pub hold_exit: Option<RouteHoldExit>,
}

/// Roomler patch (FR-148 P4 step 2, D49 rule 1): the route rank's one
/// predicate. Does the pair of a local candidate (its `local_priority`,
/// `local_component` and `local_address`) and a remote at `remote_address`
/// ride the mesh? It does when the local candidate is ranked last
/// ([`ranked_last_priority`]: a host on the mesh, a srflx mapped to it, a relay
/// relayed on it or served from it) or sits on an address `mesh` names, or
/// when the remote does: a check to a remote on the mesh goes into the mesh
/// whatever socket sends it. Only an address can be the mesh's: a name (an
/// mDNS `.local` the resolver could not see behind) is not.
///
/// The vendored agent reads it through its [`CarrierRank`]; the agent's
/// carrier record and path line read it through the agent (`Agent::route_read`),
/// so every reader names the same pairs.
pub fn rides_the_mesh(
    mesh: &dyn Fn(IpAddr) -> bool,
    local_priority: u32,
    local_component: u16,
    local_address: &str,
    remote_address: &str,
) -> bool {
    local_priority == ranked_last_priority(local_component)
        || on_the_mesh(mesh, local_address)
        || on_the_mesh(mesh, remote_address)
}

/// Is `address` (an IP, a v6 one bracketed as a URL host writes it) one `mesh`
/// names?
fn on_the_mesh(mesh: &dyn Fn(IpAddr) -> bool, address: &str) -> bool {
    address
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|ip| mesh(ip))
}

impl CarrierRank {
    /// Does this rank put a candidate on `ip` last?
    pub(crate) fn ranks_last(&self, ip: IpAddr) -> bool {
        self.mesh_last.as_ref().is_some_and(|mesh| mesh(ip))
    }

    /// Roomler patch (FR-148 P4 step 2): the route rank where it acts: set, and
    /// the mesh link not direct (`mesh_last` set). Only then does it refuse a
    /// pair or hold one; on a direct link it only keeps the follow gate and the
    /// checks after the selection off the mesh.
    pub(crate) fn route_engaged(&self) -> Option<&RouteRank> {
        self.mesh_last.as_ref().and(self.route.as_ref())
    }

    /// Roomler patch (FR-148 P4 step 2): is `address` (or a URL host) one of
    /// the route rank's mesh addresses? Never without the route rank.
    pub(crate) fn names(&self, address: &str) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| on_the_mesh(&*route.mesh, address))
    }

    /// Is `c` a local candidate the mesh carries: ranked last, or on a mesh
    /// address itself? Rule 1's local half.
    pub(crate) fn local_on_mesh(&self, c: &dyn Candidate) -> bool {
        c.priority() == ranked_last_priority(c.component()) || self.names(&c.address())
    }

    /// Roomler patch (FR-148 P4 step 2): does the pair of `local` and `remote`
    /// ride the mesh, by [`rides_the_mesh`] with the route rank's addresses?
    /// Never without the route rank.
    pub(crate) fn mesh_route(&self, local: &dyn Candidate, remote: &dyn Candidate) -> bool {
        self.route.as_ref().is_some_and(|route| {
            rides_the_mesh(
                &*route.mesh,
                local.priority(),
                local.component(),
                &local.address(),
                &remote.address(),
            )
        })
    }

    /// Roomler patch (FR-148 P4 step 2): rule 2's pair: its local candidate off
    /// the mesh, its remote on it, under the route rank where it acts. Whether
    /// the agent forms it depends on the session's sibling
    /// (`AgentInternal::refuses_pair`).
    pub(crate) fn crosses_into_the_mesh(&self, local: &dyn Candidate, remote: &dyn Candidate) -> bool {
        self.route_engaged().is_some() && self.names(&remote.address()) && !self.local_on_mesh(local)
    }

    /// Roomler patch (FR-148 P4 step 2): is a TURN server at `host` off the
    /// mesh? A name is: only an address can be the mesh's.
    pub(crate) fn turn_off_the_mesh(&self, host: &str) -> bool {
        !self.names(host)
    }
}

/// Roomler patch (FR-148 P4): the priority of a candidate the carrier rank puts
/// last: type preference 0, local preference 0. 255 for component 1.
pub fn ranked_last_priority(component: u16) -> u32 {
    256 - u32::from(component)
}

/// Collects the arguments to `ice::Agent` construction into a single structure, for
/// future-proofness of the interface.
#[derive(Default)]
pub struct AgentConfig {
    pub urls: Vec<Url>,

    /// Controls how the UDP network stack works.
    /// See [`UDPNetwork`]
    pub udp_network: UDPNetwork,

    /// It is used to perform connectivity checks. The values MUST be unguessable, with at least
    /// 128 bits of random number generator output used to generate the password, and at least 24
    /// bits of output to generate the username fragment.
    pub local_ufrag: String,
    /// It is used to perform connectivity checks. The values MUST be unguessable, with at least
    /// 128 bits of random number generator output used to generate the password, and at least 24
    /// bits of output to generate the username fragment.
    pub local_pwd: String,

    /// Controls mDNS behavior for the ICE agent.
    pub multicast_dns_mode: MulticastDnsMode,

    /// Controls the hostname for this agent. If none is specified a random one will be generated.
    pub multicast_dns_host_name: String,

    /// Control mDNS destination address
    pub multicast_dns_dest_addr: String,

    /// Defaults to 5 seconds when this property is nil.
    /// If the duration is 0, the ICE Agent will never go to disconnected.
    pub disconnected_timeout: Option<Duration>,

    /// Defaults to 25 seconds when this property is nil.
    /// If the duration is 0, we will never go to failed.
    pub failed_timeout: Option<Duration>,

    /// Determines how often should we send ICE keepalives (should be less then connectiontimeout
    /// above) when this is nil, it defaults to 10 seconds.
    /// A keepalive interval of 0 means we never send keepalive packets
    pub keepalive_interval: Option<Duration>,

    /// An optional configuration for disabling or enabling support for specific network types.
    pub network_types: Vec<NetworkType>,

    /// An optional configuration for disabling or enabling support for specific candidate types.
    pub candidate_types: Vec<CandidateType>,

    //LoggerFactory logging.LoggerFactory
    /// Controls how often our internal task loop runs when in the connecting state.
    /// Only useful for testing.
    pub check_interval: Duration,

    /// The max amount of binding requests the agent will send over a candidate pair for validation
    /// or nomination, if after max_binding_requests the candidate is yet to answer a binding
    /// request or a nomination we set the pair as failed.
    pub max_binding_requests: Option<u16>,

    pub is_controlling: bool,

    /// lite agents do not perform connectivity check and only provide host candidates.
    pub lite: bool,

    /// It is used along with nat1to1ips to specify which candidate type the 1:1 NAT IP addresses
    /// should be mapped to. If unspecified or CandidateTypeHost, nat1to1ips are used to replace
    /// host candidate IPs. If CandidateTypeServerReflexive, it will insert a srflx candidate (as
    /// if it was derived from a STUN server) with its port number being the one for the actual host
    /// candidate.  Other values will result in an error.
    pub nat_1to1_ip_candidate_type: CandidateType,

    /// Contains a list of public IP addresses that are to be used as a host candidate or srflx
    /// candidate. This is used typically for servers that are behind 1:1 D-NAT (e.g. AWS EC2
    /// instances) and to eliminate the need of server reflexisive candidate gathering.
    pub nat_1to1_ips: Vec<String>,

    /// Specify a minimum wait time before selecting host candidates.
    pub host_acceptance_min_wait: Option<Duration>,
    /// Specify a minimum wait time before selecting srflx candidates.
    pub srflx_acceptance_min_wait: Option<Duration>,
    /// Specify a minimum wait time before selecting prflx candidates.
    pub prflx_acceptance_min_wait: Option<Duration>,
    /// Specify a minimum wait time before selecting relay candidates.
    pub relay_acceptance_min_wait: Option<Duration>,

    /// Net is the our abstracted network interface for internal development purpose only
    /// (see (github.com/pion/transport/vnet)[github.com/pion/transport/vnet]).
    pub net: Option<Arc<Net>>,

    /// A function that you can use in order to whitelist or blacklist the interfaces which are
    /// used to gather ICE candidates.
    pub interface_filter: Arc<Option<InterfaceFilterFn>>,

    /// A function that you can use in order to whitelist or blacklist
    /// the ips which are used to gather ICE candidates.
    pub ip_filter: Arc<Option<IpFilterFn>>,

    /// Controls if self-signed certificates are accepted when connecting to TURN servers via TLS or
    /// DTLS.
    pub insecure_skip_verify: bool,

    /// Include loopback addresses in the candidate list.
    pub include_loopback: bool,

    /// Roomler patch (FR-148 P4): see [`CarrierRank`]. `None`, the default,
    /// changes nothing.
    pub carrier_rank: Arc<Option<CarrierRank>>,
}

impl AgentConfig {
    /// Populates an agent and falls back to defaults if fields are unset.
    pub(crate) fn init_with_defaults(&self, a: &mut AgentInternal) {
        if let Some(max_binding_requests) = self.max_binding_requests {
            a.max_binding_requests = max_binding_requests;
        } else {
            a.max_binding_requests = DEFAULT_MAX_BINDING_REQUESTS;
        }

        if let Some(host_acceptance_min_wait) = self.host_acceptance_min_wait {
            a.host_acceptance_min_wait = host_acceptance_min_wait;
        } else {
            a.host_acceptance_min_wait = DEFAULT_HOST_ACCEPTANCE_MIN_WAIT;
        }

        if let Some(srflx_acceptance_min_wait) = self.srflx_acceptance_min_wait {
            a.srflx_acceptance_min_wait = srflx_acceptance_min_wait;
        } else {
            a.srflx_acceptance_min_wait = DEFAULT_SRFLX_ACCEPTANCE_MIN_WAIT;
        }

        if let Some(prflx_acceptance_min_wait) = self.prflx_acceptance_min_wait {
            a.prflx_acceptance_min_wait = prflx_acceptance_min_wait;
        } else {
            a.prflx_acceptance_min_wait = DEFAULT_PRFLX_ACCEPTANCE_MIN_WAIT;
        }

        if let Some(relay_acceptance_min_wait) = self.relay_acceptance_min_wait {
            a.relay_acceptance_min_wait = relay_acceptance_min_wait;
        } else {
            a.relay_acceptance_min_wait = DEFAULT_RELAY_ACCEPTANCE_MIN_WAIT;
        }

        if let Some(disconnected_timeout) = self.disconnected_timeout {
            a.disconnected_timeout = disconnected_timeout;
        } else {
            a.disconnected_timeout = DEFAULT_DISCONNECTED_TIMEOUT;
        }

        if let Some(failed_timeout) = self.failed_timeout {
            a.failed_timeout = failed_timeout;
        } else {
            a.failed_timeout = DEFAULT_FAILED_TIMEOUT;
        }

        if let Some(keepalive_interval) = self.keepalive_interval {
            a.keepalive_interval = keepalive_interval;
        } else {
            a.keepalive_interval = DEFAULT_KEEPALIVE_INTERVAL;
        }

        if self.check_interval == Duration::from_secs(0) {
            a.check_interval = DEFAULT_CHECK_INTERVAL;
        } else {
            a.check_interval = self.check_interval;
        }
    }

    pub(crate) fn init_ext_ip_mapping(
        &self,
        mdns_mode: MulticastDnsMode,
        candidate_types: &[CandidateType],
    ) -> Result<Option<ExternalIpMapper>> {
        if let Some(ext_ip_mapper) =
            ExternalIpMapper::new(self.nat_1to1_ip_candidate_type, &self.nat_1to1_ips)?
        {
            if ext_ip_mapper.candidate_type == CandidateType::Host {
                if mdns_mode == MulticastDnsMode::QueryAndGather {
                    return Err(Error::ErrMulticastDnsWithNat1to1IpMapping);
                }
                let mut candi_host_enabled = false;
                for candi_type in candidate_types {
                    if *candi_type == CandidateType::Host {
                        candi_host_enabled = true;
                        break;
                    }
                }
                if !candi_host_enabled {
                    return Err(Error::ErrIneffectiveNat1to1IpMappingHost);
                }
            } else if ext_ip_mapper.candidate_type == CandidateType::ServerReflexive {
                let mut candi_srflx_enabled = false;
                for candi_type in candidate_types {
                    if *candi_type == CandidateType::ServerReflexive {
                        candi_srflx_enabled = true;
                        break;
                    }
                }
                if !candi_srflx_enabled {
                    return Err(Error::ErrIneffectiveNat1to1IpMappingSrflx);
                }
            }

            Ok(Some(ext_ip_mapper))
        } else {
            Ok(None)
        }
    }
}
