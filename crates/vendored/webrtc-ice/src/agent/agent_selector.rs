use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use stun::agent::*;
use stun::attributes::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use tokio::time::{Duration, Instant};

use crate::agent::agent_internal::*;
use crate::candidate::*;
use crate::control::*;
use crate::priority::*;
use crate::use_candidate::*;

/// Roomler patch (follow-renomination v2, 2026-07-28): how long the currently
/// selected pair may go without ANY inbound before a renomination onto an
/// overlay-remote pair is accepted as a failover. Live pairs see STUN consent
/// checks and media continuously, so 3 s of silence means the pair is dying.
const FOLLOW_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether the controlled/controlling agent should switch its selected pair
/// to a newly (re)nominated one.
///
/// rc.260 (follow everything) died in the field: Chrome nominates a
/// half-blackholed mongrel pair early — its remote is the controller's
/// overlay-TUN host candidate, whose reverse leg routes into the TUN with a
/// physical source — and following it killed SCTP ~4 s into every session.
/// rc.262 (never follow) survives but pins media to the first-nominated
/// overlay pair even when a clean real-path srflx pair sits validated and
/// idle (field 2026-07-28: video on an 89 ms churn relay while the 11 ms
/// srflx pair carried nothing; TeamViewer on the same host was smooth).
///
/// v2 follows only when it cannot be the poison case:
/// - the new pair's REMOTE parses to a real-path (non-overlay-range) IP —
///   upgrading onto srflx/public pairs is always safe; or
/// - the currently selected pair is STALE (no inbound for
///   [`FOLLOW_STALE_AFTER`]) — any nominated pair beats riding a dead one.
///
/// Unparseable remotes (mDNS names) are conservatively treated as
/// overlay-possible. Env overrides: `ROOMLER_ICE_FOLLOW_RENOMINATION=0`
/// never follows (rc.262 semantics), `=1` always follows (rc.260 semantics).
fn should_follow_renomination(
    new_remote_addr: &str,
    current: &CandidatePair,
) -> bool {
    let new_remote_is_real_path = new_remote_addr
        .parse::<std::net::IpAddr>()
        .map(|ip| !crate::agent::agent_gather::is_roomler_overlay_ip(&ip))
        .unwrap_or(false);
    let current_stale = std::time::SystemTime::now()
        .duration_since(current.remote.last_received())
        .map(|elapsed| elapsed > FOLLOW_STALE_AFTER)
        .unwrap_or(false);
    follow_renomination_policy(
        std::env::var("ROOMLER_ICE_FOLLOW_RENOMINATION").ok().as_deref(),
        new_remote_is_real_path,
        current_stale,
    )
}

/// Pure decision core of [`should_follow_renomination`] — unit-tested.
fn follow_renomination_policy(
    env: Option<&str>,
    new_remote_is_real_path: bool,
    current_stale: bool,
) -> bool {
    match env {
        Some("0") => false,
        Some("1") => true,
        _ => new_remote_is_real_path || current_stale,
    }
}

/// Roomler patch (warm standby, 2026-07-28): cadence for pinging validated
/// pairs that are NOT the selected one. Upstream keepalives only the selected
/// pair, so every other validated pair's NAT mapping expires minutes after it
/// goes idle (field, DESKTOP-69T5HUD: the agent's srflx mapping died within
/// ~4–13 min of media settling on the overlay-host pair — killing the exact
/// real-path fallback needed when the overlay carrier stalls). A controlled
/// binding request never carries USE-CANDIDATE, so a warm ping can't change
/// anyone's selection; the peer's success response refreshes the far-side
/// mapping too.
const WARM_STANDBY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

impl AgentInternal {
    /// Ping succeeded-but-unselected pairs whose local candidate has been
    /// idle past [`WARM_STANDBY_INTERVAL`]. Self-gating rides the LOCAL
    /// candidate's `last_sent` — standby pairs own distinct local candidates
    /// (srflx vs host), so selected-pair traffic doesn't mask them, and our
    /// own ping refreshes it, bounding the cadence. Capped at 4 pairs per
    /// tick. Hatch: `ROOMLER_ICE_WARM_STANDBY=0`.
    async fn warm_standby_pairs(&self) {
        if std::env::var("ROOMLER_ICE_WARM_STANDBY").as_deref() == Ok("0") {
            return;
        }
        let selected = self.agent_conn.get_selected_pair();
        let standby: Vec<_> = {
            let checklist = self.agent_conn.checklist.lock().await;
            checklist
                .iter()
                .filter(|p| p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8)
                .filter(|p| {
                    selected
                        .as_ref()
                        .is_none_or(|s| !(s.local.equal(&*p.local) && s.remote.equal(&*p.remote)))
                })
                .filter(|p| {
                    std::time::SystemTime::now()
                        .duration_since(p.local.last_sent())
                        .map(|idle| idle > WARM_STANDBY_INTERVAL)
                        .unwrap_or(true)
                })
                .take(4)
                .map(|p| (p.local.clone(), p.remote.clone()))
                .collect()
        };
        for (local, remote) in standby {
            log::trace!(
                "[{}]: warm-standby ping {} -> {}",
                self.get_name(),
                local,
                remote
            );
            ControlledSelector::ping_candidate(self, &local, &remote).await;
        }
    }
}

#[cfg(test)]
mod follow_renomination_tests {
    use super::follow_renomination_policy;

    #[test]
    fn v2_policy_matrix() {
        // Default: real-path remotes always followed; overlay remotes only
        // as a failover off a stale pair (poison-pair exclusion).
        assert!(follow_renomination_policy(None, true, false));
        assert!(follow_renomination_policy(None, true, true));
        assert!(!follow_renomination_policy(None, false, false));
        assert!(follow_renomination_policy(None, false, true));
        // Env pins: 0 = never (rc.262), 1 = always (rc.260).
        assert!(!follow_renomination_policy(Some("0"), true, true));
        assert!(follow_renomination_policy(Some("1"), false, false));
        // Garbage env falls back to the v2 rule.
        assert!(follow_renomination_policy(Some("x"), true, false));
        assert!(!follow_renomination_policy(Some("x"), false, false));
    }
}

#[async_trait]
trait ControllingSelector {
    async fn start(&self);
    async fn contact_candidates(&self);
    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    );
    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
}

#[async_trait]
trait ControlledSelector {
    async fn start(&self);
    async fn contact_candidates(&self);
    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    );
    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    );
}

impl AgentInternal {
    fn is_nominatable(&self, c: &Arc<dyn Candidate + Send + Sync>) -> bool {
        let start_time = *self.start_time.lock();
        match c.candidate_type() {
            CandidateType::Host => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.host_acceptance_min_wait.as_nanos()
            }
            CandidateType::ServerReflexive => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.srflx_acceptance_min_wait.as_nanos()
            }
            CandidateType::PeerReflexive => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.prflx_acceptance_min_wait.as_nanos()
            }
            CandidateType::Relay => {
                Instant::now()
                    .checked_duration_since(start_time)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    .as_nanos()
                    > self.relay_acceptance_min_wait.as_nanos()
            }
            CandidateType::Unspecified => {
                log::error!(
                    "is_nominatable invalid candidate type {}",
                    c.candidate_type()
                );
                false
            }
        }
    }

    async fn nominate_pair(&self) {
        let result = {
            let nominated_pair = self.nominated_pair.lock().await;
            if let Some(pair) = &*nominated_pair {
                // The controlling agent MUST include the USE-CANDIDATE attribute in
                // order to nominate a candidate pair (Section 8.1.1).  The controlled
                // agent MUST NOT include the USE-CANDIDATE attribute in a Binding
                // request.

                let (msg, result) = {
                    let ufrag_pwd = self.ufrag_pwd.lock().await;
                    let username =
                        ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
                    let mut msg = Message::new();
                    let result = msg.build(&[
                        Box::new(BINDING_REQUEST),
                        Box::new(TransactionId::new()),
                        Box::new(Username::new(ATTR_USERNAME, username)),
                        Box::<UseCandidateAttr>::default(),
                        Box::new(AttrControlling(self.tie_breaker.load(Ordering::SeqCst))),
                        Box::new(PriorityAttr(pair.local.priority())),
                        Box::new(MessageIntegrity::new_short_term_integrity(
                            ufrag_pwd.remote_pwd.clone(),
                        )),
                        Box::new(FINGERPRINT),
                    ]);
                    (msg, result)
                };

                if let Err(err) = result {
                    // Roomler vendor patch (2026-05-18 a third field-test host log-flood
                    // fix): downgrade per-attempt STUN build failures
                    // to DEBUG so they don't drown the rolling log,
                    // and prefix with a target so they're greppable.
                    // Per-candidate-pair build failures are not
                    // session-fatal — ICE keeps trying other pairs.
                    log::debug!(target: "webrtc_ice::nominate_pair", "STUN message build failed: {}", err);
                    None
                } else {
                    log::trace!(
                        "ping STUN (nominate candidate pair from {} to {}",
                        pair.local,
                        pair.remote
                    );
                    let local = pair.local.clone();
                    let remote = pair.remote.clone();
                    Some((msg, local, remote))
                }
            } else {
                None
            }
        };

        if let Some((msg, local, remote)) = result {
            self.send_binding_request(&msg, &local, &remote).await;
        }
    }

    pub(crate) async fn start(&self) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::start(self).await;
        } else {
            ControlledSelector::start(self).await;
        }
    }

    pub(crate) async fn contact_candidates(&self) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::contact_candidates(self).await;
        } else {
            ControlledSelector::contact_candidates(self).await;
        }
    }

    pub(crate) async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::ping_candidate(self, local, remote).await;
        } else {
            ControlledSelector::ping_candidate(self, local, remote).await;
        }
    }

    pub(crate) async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::handle_success_response(self, m, local, remote, remote_addr).await;
        } else {
            ControlledSelector::handle_success_response(self, m, local, remote, remote_addr).await;
        }
    }

    pub(crate) async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.is_controlling.load(Ordering::SeqCst) {
            ControllingSelector::handle_binding_request(self, m, local, remote).await;
        } else {
            ControlledSelector::handle_binding_request(self, m, local, remote).await;
        }
    }
}

#[async_trait]
impl ControllingSelector for AgentInternal {
    async fn start(&self) {
        {
            let mut nominated_pair = self.nominated_pair.lock().await;
            *nominated_pair = None;
        }
        *self.start_time.lock() = Instant::now();
    }

    async fn contact_candidates(&self) {
        // A lite selector should not contact candidates
        if self.lite.load(Ordering::SeqCst) {
            // This only happens if both peers are lite. See RFC 8445 S6.1.1 and S6.2
            log::trace!("now falling back to full agent");
        }

        let nominated_pair_is_some = {
            let nominated_pair = self.nominated_pair.lock().await;
            nominated_pair.is_some()
        };

        if self.agent_conn.get_selected_pair().is_some() {
            if self.validate_selected_pair().await {
                log::trace!("[{}]: checking keepalive", self.get_name());
                self.check_keepalive().await;
            }
        } else if nominated_pair_is_some {
            self.nominate_pair().await;
        } else {
            let has_nominated_pair =
                if let Some(p) = self.agent_conn.get_best_valid_candidate_pair().await {
                    self.is_nominatable(&p.local) && self.is_nominatable(&p.remote)
                } else {
                    false
                };

            if has_nominated_pair {
                if let Some(p) = self.agent_conn.get_best_valid_candidate_pair().await {
                    log::trace!(
                        "Nominatable pair found, nominating ({}, {})",
                        p.local.to_string(),
                        p.remote.to_string()
                    );
                    p.nominated.store(true, Ordering::SeqCst);
                    {
                        let mut nominated_pair = self.nominated_pair.lock().await;
                        *nominated_pair = Some(p);
                    }
                }

                self.nominate_pair().await;
            } else {
                self.ping_all_candidates().await;
            }
        }
    }

    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let (msg, result) = {
            let ufrag_pwd = self.ufrag_pwd.lock().await;
            let username = ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
            let mut msg = Message::new();
            let result = msg.build(&[
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(AttrControlling(self.tie_breaker.load(Ordering::SeqCst))),
                Box::new(PriorityAttr(local.priority())),
                Box::new(MessageIntegrity::new_short_term_integrity(
                    ufrag_pwd.remote_pwd.clone(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (msg, result)
        };

        if let Err(err) = result {
            // Roomler vendor patch (2026-05-18 a third field-test host log-flood fix):
            // see nominate_pair above; per-candidate STUN build
            // failures are not session-fatal.
            log::debug!(target: "webrtc_ice::ping_candidate", "STUN message build failed: {}", err);
        } else {
            self.send_binding_request(&msg, local, remote).await;
        }
    }

    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        if let Some(pending_request) = self.handle_inbound_binding_success(m.transaction_id).await {
            let transaction_addr = pending_request.destination;

            // Assert that NAT is not symmetric
            // https://tools.ietf.org/html/rfc8445#section-7.2.5.2.1
            if transaction_addr != remote_addr {
                log::debug!("discard message: transaction source and destination does not match expected({}), actual({})", transaction_addr, remote);
                return;
            }

            log::trace!(
                "inbound STUN (SuccessResponse) from {} to {}",
                remote,
                local
            );
            let selected_pair_is_none = self.agent_conn.get_selected_pair().is_none();

            if let Some(p) = self.find_pair(local, remote).await {
                p.state
                    .store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
                log::trace!(
                    "Found valid candidate pair: {}, p.state: {}, isUseCandidate: {}, {}",
                    p,
                    p.state.load(Ordering::SeqCst),
                    pending_request.is_use_candidate,
                    selected_pair_is_none
                );
                // Roomler patch (follow renomination): mirror of the
                // handle_binding_request policy — when the USE-CANDIDATE
                // arrived before this pair's own check completed, the
                // triggered check's success must also honour the controlling
                // agent's latest nomination, not only a cold start.
                // Default-OFF (2026-07-27 field): following renominations let Chrome
                // steer the agent onto a half-blackholed pair (browser-TUN-host ×
                // agent-srflx validates one-way through the home-NAT masquerade;
                // the reverse direction routes into the TUN with a physical
                // source) — SCTP died ~4 s in, every session. Opt-in via
                // ROOMLER_ICE_FOLLOW_RENOMINATION=1 until the follow policy
                // validates pair liveness both ways.
                // v2 (2026-07-28): policy in should_follow_renomination() —
                // real-path remotes always, overlay remotes only off a stale
                // pair. Env: 0=never (rc.262), 1=always (rc.260).
                let follow_renomination = {
                    let selected = self.agent_conn.get_selected_pair();
                    selected
                        .as_ref()
                        .is_some_and(|s| should_follow_renomination(&p.remote.address(), s))
                };
                let already_selected = {
                    let selected = self.agent_conn.get_selected_pair();
                    selected
                        .as_ref()
                        .is_some_and(|s| s.local.equal(&*p.local) && s.remote.equal(&*p.remote))
                };
                if pending_request.is_use_candidate
                    && (selected_pair_is_none || (follow_renomination && !already_selected))
                {
                    self.set_selected_pair(Some(Arc::clone(&p))).await;
                }
            } else {
                // This shouldn't happen
                log::error!("Success response from invalid candidate pair");
            }
        } else {
            log::warn!(
                "discard message from ({}), unknown TransactionID 0x{:?}",
                remote,
                m.transaction_id
            );
        }
    }

    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        self.send_binding_success(m, local, remote).await;
        log::trace!("controllingSelector: sendBindingSuccess");

        if let Some(p) = self.find_pair(local, remote).await {
            let nominated_pair_is_none = {
                let nominated_pair = self.nominated_pair.lock().await;
                nominated_pair.is_none()
            };

            log::trace!(
                "controllingSelector: after findPair {}, p.state: {}, {}",
                p,
                p.state.load(Ordering::SeqCst),
                nominated_pair_is_none,
                //self.agent_conn.get_selected_pair().await.is_none() //, {}
            );
            if p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8
                && nominated_pair_is_none
                && self.agent_conn.get_selected_pair().is_none()
            {
                if let Some(best_pair) = self.agent_conn.get_best_available_candidate_pair().await {
                    log::trace!(
                        "controllingSelector: getBestAvailableCandidatePair {}",
                        best_pair
                    );
                    if best_pair == p
                        && self.is_nominatable(&p.local)
                        && self.is_nominatable(&p.remote)
                    {
                        log::trace!("The candidate ({}, {}) is the best candidate available, marking it as nominated",
                            p.local, p.remote);
                        {
                            let mut nominated_pair = self.nominated_pair.lock().await;
                            *nominated_pair = Some(p);
                        }
                        self.nominate_pair().await;
                    }
                } else {
                    log::trace!("No best pair available");
                }
            }
        } else {
            log::trace!("controllingSelector: addPair");
            self.add_pair(local.clone(), remote.clone()).await;
        }
    }
}

#[async_trait]
impl ControlledSelector for AgentInternal {
    async fn start(&self) {}

    async fn contact_candidates(&self) {
        // A lite selector should not contact candidates
        if self.lite.load(Ordering::SeqCst) {
            self.validate_selected_pair().await;
        } else if self.agent_conn.get_selected_pair().is_some() {
            if self.validate_selected_pair().await {
                log::trace!("[{}]: checking keepalive", self.get_name());
                self.check_keepalive().await;
                self.warm_standby_pairs().await;
            }
        } else {
            self.ping_all_candidates().await;
        }
    }

    async fn ping_candidate(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let (msg, result) = {
            let ufrag_pwd = self.ufrag_pwd.lock().await;
            let username = ufrag_pwd.remote_ufrag.clone() + ":" + ufrag_pwd.local_ufrag.as_str();
            let mut msg = Message::new();
            let result = msg.build(&[
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(AttrControlled(self.tie_breaker.load(Ordering::SeqCst))),
                Box::new(PriorityAttr(local.priority())),
                Box::new(MessageIntegrity::new_short_term_integrity(
                    ufrag_pwd.remote_pwd.clone(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (msg, result)
        };

        if let Err(err) = result {
            // Roomler vendor patch (2026-05-18 a third field-test host log-flood fix):
            // see nominate_pair above; per-candidate STUN build
            // failures are not session-fatal.
            log::debug!(target: "webrtc_ice::ping_candidate", "STUN message build failed: {}", err);
        } else {
            self.send_binding_request(&msg, local, remote).await;
        }
    }

    async fn handle_success_response(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
        remote_addr: SocketAddr,
    ) {
        // https://tools.ietf.org/html/rfc8445#section-7.3.1.5
        // If the controlled agent does not accept the request from the
        // controlling agent, the controlled agent MUST reject the nomination
        // request with an appropriate error code response (e.g., 400)
        // [RFC5389].

        if let Some(pending_request) = self.handle_inbound_binding_success(m.transaction_id).await {
            let transaction_addr = pending_request.destination;

            // Assert that NAT is not symmetric
            // https://tools.ietf.org/html/rfc8445#section-7.2.5.2.1
            if transaction_addr != remote_addr {
                log::debug!("discard message: transaction source and destination does not match expected({}), actual({})", transaction_addr, remote);
                return;
            }

            log::trace!(
                "inbound STUN (SuccessResponse) from {} to {}",
                remote,
                local
            );

            if let Some(p) = self.find_pair(local, remote).await {
                p.state
                    .store(CandidatePairState::Succeeded as u8, Ordering::SeqCst);
                log::trace!("Found valid candidate pair: {}", p);
            } else {
                // This shouldn't happen
                log::error!("Success response from invalid candidate pair");
            }
        } else {
            log::warn!(
                "discard message from ({}), unknown TransactionID 0x{:?}",
                remote,
                m.transaction_id
            );
        }
    }

    async fn handle_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if self.find_pair(local, remote).await.is_none() {
            self.add_pair(local.clone(), remote.clone()).await;
        }

        if let Some(p) = self.find_pair(local, remote).await {
            let use_candidate = m.contains(ATTR_USE_CANDIDATE);
            if use_candidate {
                // https://tools.ietf.org/html/rfc8445#section-7.3.1.5

                if p.state.load(Ordering::SeqCst) == CandidatePairState::Succeeded as u8 {
                    // If the state of this pair is Succeeded, it means that the check
                    // previously sent by this pair produced a successful response and
                    // generated a valid pair (Section 7.2.5.3.2).  The agent sets the
                    // nominated flag value of the valid pair to true.
                    //
                    // Roomler patch (follow renomination): upstream only set the
                    // selected pair when NONE was set, pinning the controlled
                    // agent to the FIRST nominated pair for the connection's
                    // lifetime. Browsers renominate deliberately when they
                    // switch transports (ICE renomination semantics) — e.g.
                    // Chrome moving off a deprioritized overlay-TUN host pair
                    // onto the real-path srflx pair. Follow the controlling
                    // agent's most recent nomination so both directions ride
                    // the pair it chose; without this the browser switches its
                    // send path while the agent keeps streaming media over the
                    // stale pair (field 2026-07-27, NEO16→69T5HUD: video pinned
                    // to the overlay pair through carrier churn while the
                    // nominated srflx pair sat idle).
                    // Default-OFF (2026-07-27 field): following renominations let
                    // Chrome steer the agent onto a half-blackholed pair
                    // (browser-TUN-host × agent-srflx validates one-way through
                    // the home-NAT masquerade; the reverse direction routes into
                    // the TUN with a physical source) — SCTP died ~4 s in, every
                    // session. Opt-in via ROOMLER_ICE_FOLLOW_RENOMINATION=1 until
                    // the follow policy validates pair liveness both ways.
                    // v2 (2026-07-28): see should_follow_renomination().
                    let follow_renomination = {
                        let selected = self.agent_conn.get_selected_pair();
                        selected
                            .as_ref()
                            .is_some_and(|s| should_follow_renomination(&p.remote.address(), s))
                    };
                    let already_selected = {
                        let selected = self.agent_conn.get_selected_pair();
                        selected.as_ref().is_some_and(|s| {
                            s.local.equal(&*p.local) && s.remote.equal(&*p.remote)
                        })
                    };
                    if self.agent_conn.get_selected_pair().is_none()
                        || (follow_renomination && !already_selected)
                    {
                        self.set_selected_pair(Some(Arc::clone(&p))).await;
                    }
                    self.send_binding_success(m, local, remote).await;
                } else {
                    // If the received Binding request triggered a new check to be
                    // enqueued in the triggered-check queue (Section 7.3.1.4), once the
                    // check is sent and if it generates a successful response, and
                    // generates a valid pair, the agent sets the nominated flag of the
                    // pair to true.  If the request fails (Section 7.2.5.2), the agent
                    // MUST remove the candidate pair from the valid list, set the
                    // candidate pair state to Failed, and set the checklist state to
                    // Failed.
                    self.ping_candidate(local, remote).await;
                }
            } else {
                self.send_binding_success(m, local, remote).await;
                self.ping_candidate(local, remote).await;
            }
        }
    }
}
