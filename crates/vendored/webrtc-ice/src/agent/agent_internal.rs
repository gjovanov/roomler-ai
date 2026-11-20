use portable_atomic::{AtomicBool, AtomicU64};

use arc_swap::ArcSwapOption;
use util::sync::Mutex as SyncMutex;

use super::agent_transport::*;
use super::*;
use crate::candidate::candidate_base::CandidateBaseConfig;
use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::util::*;

/// VENDOR PATCH (#3716): a local candidate's close still running after this
/// long is named in a warning (see `delete_all_candidates`).
const SLOW_CANDIDATE_CLOSE: Duration = Duration::from_secs(5);

pub type ChanCandidateTx =
    Arc<Mutex<Option<mpsc::Sender<Option<Arc<dyn Candidate + Send + Sync>>>>>>;

#[derive(Default)]
pub(crate) struct UfragPwd {
    pub(crate) local_ufrag: String,
    pub(crate) local_pwd: String,
    pub(crate) remote_ufrag: String,
    pub(crate) remote_pwd: String,
}

pub struct AgentInternal {
    // State owned by the taskLoop
    pub(crate) on_connected_tx: Mutex<Option<mpsc::Sender<()>>>,
    pub(crate) on_connected_rx: Mutex<Option<mpsc::Receiver<()>>>,

    // State for closing
    pub(crate) done_tx: Mutex<Option<mpsc::Sender<()>>>,
    // VENDOR PATCH (#1740): raised by close() before its candidate sweep and
    // never lowered. The mDNS resolutions end on it, and add_candidate()
    // reads it under the local-candidates lock to turn away a candidate
    // gathered after the close.
    pub(crate) closed: watch::Sender<bool>,
    // force candidate to be contacted immediately (instead of waiting for task ticker)
    pub(crate) force_candidate_contact_tx: mpsc::Sender<bool>,
    pub(crate) done_and_force_candidate_contact_rx:
        Mutex<Option<(mpsc::Receiver<()>, mpsc::Receiver<bool>)>>,

    pub(crate) chan_candidate_tx: ChanCandidateTx,
    pub(crate) chan_candidate_pair_tx: Mutex<Option<mpsc::Sender<()>>>,
    pub(crate) chan_state_tx: Mutex<Option<mpsc::Sender<ConnectionState>>>,

    pub(crate) on_connection_state_change_hdlr: ArcSwapOption<Mutex<OnConnectionStateChangeHdlrFn>>,
    pub(crate) on_selected_candidate_pair_change_hdlr:
        ArcSwapOption<Mutex<OnSelectedCandidatePairChangeHdlrFn>>,
    pub(crate) on_candidate_hdlr: ArcSwapOption<Mutex<OnCandidateHdlrFn>>,

    pub(crate) tie_breaker: AtomicU64,
    pub(crate) is_controlling: AtomicBool,
    pub(crate) lite: AtomicBool,

    pub(crate) start_time: SyncMutex<Instant>,
    pub(crate) nominated_pair: Mutex<Option<Arc<CandidatePair>>>,

    pub(crate) connection_state: AtomicU8, //ConnectionState,

    pub(crate) started_ch_tx: Mutex<Option<broadcast::Sender<()>>>,

    pub(crate) ufrag_pwd: Mutex<UfragPwd>,

    pub(crate) local_candidates: Mutex<HashMap<NetworkType, Vec<Arc<dyn Candidate + Send + Sync>>>>,
    pub(crate) remote_candidates:
        Mutex<HashMap<NetworkType, Vec<Arc<dyn Candidate + Send + Sync>>>>,

    // LRU of outbound Binding request Transaction IDs
    pub(crate) pending_binding_requests: Mutex<Vec<BindingRequest>>,

    pub(crate) agent_conn: Arc<AgentConn>,

    // the following variables won't be changed after init_with_defaults()
    pub(crate) insecure_skip_verify: bool,
    pub(crate) max_binding_requests: u16,
    pub(crate) host_acceptance_min_wait: Duration,
    pub(crate) srflx_acceptance_min_wait: Duration,
    pub(crate) prflx_acceptance_min_wait: Duration,
    pub(crate) relay_acceptance_min_wait: Duration,
    // How long connectivity checks can fail before the ICE Agent
    // goes to disconnected
    pub(crate) disconnected_timeout: Duration,
    // How long connectivity checks can fail before the ICE Agent
    // goes to failed
    pub(crate) failed_timeout: Duration,
    // How often should we send keepalive packets?
    // 0 means never
    pub(crate) keepalive_interval: Duration,
    // How often should we run our internal taskLoop to check for state changes when connecting
    pub(crate) check_interval: Duration,
    // Roomler patch (FR-148 P4): the carrier rank, read where a candidate is
    // made and by the controlled agent's checks after its first selection.
    pub(crate) carrier_rank: Arc<Option<CarrierRank>>,
    // Roomler patch (FR-148 P4 step 2): the route rank's hold
    // (`holds_mesh_route`): when the checks started and when and why the hold
    // stopped, and how many allocations on a TURN server off the mesh are
    // still running. Zero without the route rank where it acts.
    pub(crate) route_hold: SyncMutex<RouteHoldState>,
    pub(crate) real_turns_pending: AtomicUsize,
    // Roomler patch (FR-148 P4 step 2): rule 2's sibling (R1), as
    // `SIBLING_TURN` and `SIBLING_HOST` bits (`refuses_pair`). Zero without the
    // route rank where it acts.
    pub(crate) mesh_sibling: AtomicU8,
}

/// Roomler patch (FR-148 P4 step 2): rule 2's sibling, from the config: a TURN
/// URL on the mesh, with relay gathering on. Its relay is served from the mesh
/// and the srflx its server answers is mapped onto it: both are ranked last.
/// Dropped when the whole gather ends without a srflx or relay candidate on
/// the mesh (`settle_sibling`).
pub(crate) const SIBLING_TURN: u8 = 1;

/// Roomler patch (FR-148 P4 step 2): rule 2's sibling, from the interfaces: the
/// host gather will make a candidate on a mesh address (`Agent::new` reads the
/// gather's own interface list, with its filters). Dropped when the host
/// gather ends without one (`settle_sibling`).
pub(crate) const SIBLING_HOST: u8 = 2;

/// Roomler patch (FR-148 P4 step 2): the route rank's hold, as it went.
#[derive(Default)]
pub(crate) struct RouteHoldState {
    /// When the checks started.
    pub(crate) started: Option<Instant>,
    /// When and why the hold last stopped holding. Cleared if it holds again:
    /// a pair off the mesh formed before any selection (a late remote relay).
    pub(crate) stopped: Option<(Instant, RouteHoldExit)>,
    /// Over for good: a pair is selected, or the bound passed. It never holds
    /// again, so a pair that carries the session is always answered.
    pub(crate) over: bool,
}

/// Roomler patch (FR-148 P4 step 2): under the route rank where it acts, with
/// relay gathering on, how many of the config's TURN URLs are off the mesh
/// (`off_the_mesh`) or on it; zero otherwise. A URL host that is a name is off
/// the mesh: only an address can be the mesh's.
fn route_turns(config: &AgentConfig, off_the_mesh: bool) -> usize {
    let Some(rank) = (*config.carrier_rank)
        .as_ref()
        .filter(|rank| rank.route_engaged().is_some())
    else {
        return 0;
    };
    if !(config.candidate_types.is_empty()
        || config.candidate_types.contains(&CandidateType::Relay))
    {
        return 0;
    }
    config
        .urls
        .iter()
        .filter(|url| {
            matches!(url.scheme, SchemeType::Turn | SchemeType::Turns)
                && rank.turn_off_the_mesh(&url.host) == off_the_mesh
        })
        .count()
}

impl AgentInternal {
    pub(super) fn new(config: &AgentConfig) -> (Self, ChanReceivers) {
        let (chan_state_tx, chan_state_rx) = mpsc::channel(1);
        let (chan_candidate_tx, chan_candidate_rx) = mpsc::channel(1);
        let (chan_candidate_pair_tx, chan_candidate_pair_rx) = mpsc::channel(1);
        let (on_connected_tx, on_connected_rx) = mpsc::channel(1);
        let (done_tx, done_rx) = mpsc::channel(1);
        let (force_candidate_contact_tx, force_candidate_contact_rx) = mpsc::channel(1);
        let (started_ch_tx, _) = broadcast::channel(1);

        let ai = AgentInternal {
            on_connected_tx: Mutex::new(Some(on_connected_tx)),
            on_connected_rx: Mutex::new(Some(on_connected_rx)),

            done_tx: Mutex::new(Some(done_tx)),
            closed: watch::channel(false).0,
            force_candidate_contact_tx,
            done_and_force_candidate_contact_rx: Mutex::new(Some((
                done_rx,
                force_candidate_contact_rx,
            ))),

            chan_candidate_tx: Arc::new(Mutex::new(Some(chan_candidate_tx))),
            chan_candidate_pair_tx: Mutex::new(Some(chan_candidate_pair_tx)),
            chan_state_tx: Mutex::new(Some(chan_state_tx)),

            on_connection_state_change_hdlr: ArcSwapOption::empty(),
            on_selected_candidate_pair_change_hdlr: ArcSwapOption::empty(),
            on_candidate_hdlr: ArcSwapOption::empty(),

            tie_breaker: AtomicU64::new(rand::random::<u64>()),
            is_controlling: AtomicBool::new(config.is_controlling),
            lite: AtomicBool::new(config.lite),

            start_time: SyncMutex::new(Instant::now()),
            nominated_pair: Mutex::new(None),

            connection_state: AtomicU8::new(ConnectionState::New as u8),

            insecure_skip_verify: config.insecure_skip_verify,

            started_ch_tx: Mutex::new(Some(started_ch_tx)),

            //won't change after init_with_defaults()
            max_binding_requests: 0,
            host_acceptance_min_wait: Duration::from_secs(0),
            srflx_acceptance_min_wait: Duration::from_secs(0),
            prflx_acceptance_min_wait: Duration::from_secs(0),
            relay_acceptance_min_wait: Duration::from_secs(0),

            // How long connectivity checks can fail before the ICE Agent
            // goes to disconnected
            disconnected_timeout: Duration::from_secs(0),

            // How long connectivity checks can fail before the ICE Agent
            // goes to failed
            failed_timeout: Duration::from_secs(0),

            // How often should we send keepalive packets?
            // 0 means never
            keepalive_interval: Duration::from_secs(0),

            // How often should we run our internal taskLoop to check for state changes when connecting
            check_interval: Duration::from_secs(0),

            ufrag_pwd: Mutex::new(UfragPwd::default()),

            local_candidates: Mutex::new(HashMap::new()),
            remote_candidates: Mutex::new(HashMap::new()),

            // LRU of outbound Binding request Transaction IDs
            pending_binding_requests: Mutex::new(vec![]),

            // AgentConn
            agent_conn: Arc::new(AgentConn::new()),

            carrier_rank: Arc::clone(&config.carrier_rank),
            route_hold: SyncMutex::new(RouteHoldState::default()),
            real_turns_pending: AtomicUsize::new(route_turns(config, true)),
            mesh_sibling: AtomicU8::new(if route_turns(config, false) > 0 {
                SIBLING_TURN
            } else {
                0
            }),
        };

        let chan_receivers = ChanReceivers {
            chan_state_rx,
            chan_candidate_rx,
            chan_candidate_pair_rx,
        };
        (ai, chan_receivers)
    }
    pub(crate) async fn start_connectivity_checks(
        self: &Arc<Self>,
        is_controlling: bool,
        remote_ufrag: String,
        remote_pwd: String,
    ) -> Result<()> {
        {
            let started_ch_tx = self.started_ch_tx.lock().await;
            if started_ch_tx.is_none() {
                return Err(Error::ErrMultipleStart);
            }
        }

        log::debug!(
            "Started agent: isControlling? {}, remoteUfrag: {}, remotePwd: {}",
            is_controlling,
            remote_ufrag,
            remote_pwd
        );
        self.set_remote_credentials(remote_ufrag, remote_pwd)
            .await?;
        self.is_controlling.store(is_controlling, Ordering::SeqCst);
        self.start().await;
        {
            let mut started_ch_tx = self.started_ch_tx.lock().await;
            started_ch_tx.take();
        }

        self.update_connection_state(ConnectionState::Checking)
            .await;

        // Roomler patch (FR-148 P4 step 2): the route rank's hold counts from here.
        self.route_hold.lock().started = Some(Instant::now());
        self.request_connectivity_check();

        self.connectivity_checks().await;

        Ok(())
    }

    async fn contact(
        &self,
        last_connection_state: &mut ConnectionState,
        checking_duration: &mut Instant,
    ) {
        if self.connection_state.load(Ordering::SeqCst) == ConnectionState::Failed as u8 {
            // The connection is currently failed so don't send any checks
            // In the future it may be restarted though
            *last_connection_state = self.connection_state.load(Ordering::SeqCst).into();
            return;
        }
        if self.connection_state.load(Ordering::SeqCst) == ConnectionState::Checking as u8 {
            // We have just entered checking for the first time so update our checking timer
            if *last_connection_state as u8 != self.connection_state.load(Ordering::SeqCst) {
                *checking_duration = Instant::now();
            }

            // We have been in checking longer then Disconnect+Failed timeout, set the connection to Failed
            if Instant::now()
                .checked_duration_since(*checking_duration)
                .unwrap_or_else(|| Duration::from_secs(0))
                > self.disconnected_timeout + self.failed_timeout
            {
                self.update_connection_state(ConnectionState::Failed).await;
                *last_connection_state = self.connection_state.load(Ordering::SeqCst).into();
                return;
            }
        }

        self.contact_candidates().await;

        *last_connection_state = self.connection_state.load(Ordering::SeqCst).into();
    }

    async fn connectivity_checks(self: &Arc<Self>) {
        const ZERO_DURATION: Duration = Duration::from_secs(0);
        let mut last_connection_state = ConnectionState::Unspecified;
        let mut checking_duration = Instant::now();
        let (check_interval, keepalive_interval, disconnected_timeout, failed_timeout) = (
            self.check_interval,
            self.keepalive_interval,
            self.disconnected_timeout,
            self.failed_timeout,
        );

        let done_and_force_candidate_contact_rx = {
            let mut done_and_force_candidate_contact_rx =
                self.done_and_force_candidate_contact_rx.lock().await;
            done_and_force_candidate_contact_rx.take()
        };

        if let Some((mut done_rx, mut force_candidate_contact_rx)) =
            done_and_force_candidate_contact_rx
        {
            let ai = Arc::clone(self);
            tokio::spawn(async move {
                loop {
                    let mut interval = DEFAULT_CHECK_INTERVAL;

                    let mut update_interval = |x: Duration| {
                        if x != ZERO_DURATION && (interval == ZERO_DURATION || interval > x) {
                            interval = x;
                        }
                    };

                    match last_connection_state {
                        ConnectionState::New | ConnectionState::Checking => {
                            // While connecting, check candidates more frequently
                            update_interval(check_interval);
                        }
                        ConnectionState::Connected | ConnectionState::Disconnected => {
                            update_interval(keepalive_interval);
                        }
                        _ => {}
                    };
                    // Ensure we run our task loop as quickly as the minimum of our various configured timeouts
                    update_interval(disconnected_timeout);
                    update_interval(failed_timeout);

                    let t = tokio::time::sleep(interval);
                    tokio::pin!(t);

                    tokio::select! {
                        _ = t.as_mut() => {
                            ai.contact(&mut last_connection_state, &mut checking_duration).await;
                        },
                        _ = force_candidate_contact_rx.recv() => {
                            ai.contact(&mut last_connection_state, &mut checking_duration).await;
                        },
                        _ = done_rx.recv() => {
                            return;
                        }
                    }
                }
            });
        }
    }

    pub(crate) async fn update_connection_state(&self, new_state: ConnectionState) {
        if self.connection_state.load(Ordering::SeqCst) != new_state as u8 {
            // Connection has gone to failed, release all gathered candidates
            if new_state == ConnectionState::Failed {
                self.delete_all_candidates().await;
            }

            log::info!(
                "[{}]: Setting new connection state: {}",
                self.get_name(),
                new_state
            );
            self.connection_state
                .store(new_state as u8, Ordering::SeqCst);

            // Call handler after finishing current task since we may be holding the agent lock
            // and the handler may also require it
            {
                let chan_state_tx = self.chan_state_tx.lock().await;
                if let Some(tx) = &*chan_state_tx {
                    let _ = tx.send(new_state).await;
                }
            }
        }
    }

    pub(crate) async fn set_selected_pair(&self, p: Option<Arc<CandidatePair>>) {
        log::trace!(
            "[{}]: Set selected candidate pair: {:?}",
            self.get_name(),
            p
        );

        if let Some(p) = p {
            // Roomler patch (observability, 2026-07-28): selection changes at
            // WARN — the agent's default tracing filter uploads `warn`-level
            // events from this target fleet-wide, so which path media rides
            // (and every mid-session migration) becomes visible in the
            // centralized logs instead of needing browser-side getStats
            // forensics. One line per actual change, never per tick.
            let prev = self.agent_conn.get_selected_pair();
            let changed = prev
                .as_ref()
                .is_none_or(|o| !(o.local.equal(&*p.local) && o.remote.equal(&*p.remote)));
            if changed {
                log::warn!(
                    "[{}]: selected pair changed: {} -> {}",
                    self.get_name(),
                    prev.as_ref()
                        .map(|o| o.to_string())
                        .unwrap_or_else(|| "(none)".to_owned()),
                    p
                );
            }
            p.nominated.store(true, Ordering::SeqCst);
            // Roomler patch (FR-148 P4 step 2): a selection ends the hold.
            self.hold_ends_at_selection(&p);
            self.agent_conn.selected_pair.store(Some(p));

            self.update_connection_state(ConnectionState::Connected)
                .await;

            // Notify when the selected pair changes
            {
                let chan_candidate_pair_tx = self.chan_candidate_pair_tx.lock().await;
                if let Some(tx) = &*chan_candidate_pair_tx {
                    let _ = tx.send(()).await;
                }
            }

            // Signal connected
            {
                let mut on_connected_tx = self.on_connected_tx.lock().await;
                on_connected_tx.take();
            }
        } else {
            self.agent_conn.selected_pair.store(None);
        }
    }

    pub(crate) async fn ping_all_candidates(&self) {
        log::trace!("[{}]: pinging all candidates", self.get_name(),);

        // Roomler patch (FR-148 P4 step 2): read before the checklist's lock,
        // which the hold's own read takes.
        let held = self.holds_mesh_route().await;
        let rank = (*self.carrier_rank).as_ref();

        let mut pairs: Vec<(
            Arc<dyn Candidate + Send + Sync>,
            Arc<dyn Candidate + Send + Sync>,
        )> = vec![];

        {
            let mut checklist = self.agent_conn.checklist.lock().await;
            if checklist.is_empty() {
                log::warn!(
                    "[{}]: pingAllCandidates called with no candidate pairs. Connection is not possible yet.",
                    self.get_name(),
                );
            }
            for p in &mut *checklist {
                // Roomler patch (FR-148 P4 step 2): a held pair is not checked.
                if held && rank.is_some_and(|rank| rank.mesh_route(&*p.local, &*p.remote)) {
                    continue;
                }
                let p_state = p.state.load(Ordering::SeqCst);
                if p_state == CandidatePairState::Waiting as u8 {
                    p.state
                        .store(CandidatePairState::InProgress as u8, Ordering::SeqCst);
                } else if p_state != CandidatePairState::InProgress as u8 {
                    continue;
                }

                if p.binding_request_count.load(Ordering::SeqCst) > self.max_binding_requests {
                    log::trace!(
                        "[{}]: max requests reached for pair {}, marking it as failed",
                        self.get_name(),
                        p
                    );
                    p.state
                        .store(CandidatePairState::Failed as u8, Ordering::SeqCst);
                } else {
                    p.binding_request_count.fetch_add(1, Ordering::SeqCst);
                    let local = p.local.clone();
                    let remote = p.remote.clone();
                    pairs.push((local, remote));
                }
            }
        }

        for (local, remote) in pairs {
            self.ping_candidate(&local, &remote).await;
        }
    }

    pub(crate) async fn add_pair(
        &self,
        local: Arc<dyn Candidate + Send + Sync>,
        remote: Arc<dyn Candidate + Send + Sync>,
    ) {
        // Roomler patch (FR-148 P4 step 2): rule 2, a pair into the mesh is not
        // formed while a sibling carries its route ([`CarrierRank::route`]).
        if self.refuses_pair(&*local, &*remote) {
            log::debug!(
                "[{}]: no pair {} -> {}: it crosses into the mesh",
                self.get_name(),
                local,
                remote
            );
            return;
        }
        // Roomler patch (FR-148 P4 step 2): where the route rank acts, a pair
        // is formed once, though the pairs a dropped sibling had refused are
        // formed by `settle_sibling` while the candidates still trickle.
        let once = (*self.carrier_rank)
            .as_ref()
            .is_some_and(|rank| rank.route_engaged().is_some());
        let p = Arc::new(CandidatePair::new(
            local,
            remote,
            self.is_controlling.load(Ordering::SeqCst),
        ));
        let mut checklist = self.agent_conn.checklist.lock().await;
        if once
            && checklist
                .iter()
                .any(|q| q.local.equal(&*p.local) && q.remote.equal(&*p.remote))
        {
            return;
        }
        checklist.push(p);
    }

    /// Roomler patch (FR-148 P4 step 2, D49 rule 2 with R1): does the agent
    /// refuse the pair of `local` and `remote`? Only a pair into the mesh
    /// ([`CarrierRank::crosses_into_the_mesh`]), and only while the session has
    /// a sibling that carries the same route ranked: a TURN URL on the mesh, or
    /// a host candidate on it (`mesh_sibling`). The sibling is fixed before any
    /// candidate is gathered or any remote arrives, so the answer never depends
    /// on the order they come in; it is dropped only when the gather that was
    /// to make it has ended without it (`settle_sibling`). Without one the pair
    /// is the route's only pair: it is formed, and the hold treats it as any
    /// pair the mesh carries.
    pub(crate) fn refuses_pair(&self, local: &dyn Candidate, remote: &dyn Candidate) -> bool {
        self.mesh_sibling.load(Ordering::SeqCst) != 0
            && (*self.carrier_rank)
                .as_ref()
                .is_some_and(|rank| rank.crosses_into_the_mesh(local, remote))
    }

    /// Roomler patch (FR-148 P4 step 2): the gather that was to make the
    /// sibling `bit` has ended: the host gather for `SIBLING_HOST`, the whole
    /// gather for `SIBLING_TURN`. When it made no candidate on the mesh of its
    /// kind (a host for the one; a srflx or relay for the other: a bind on the
    /// mesh address failed, the TURN server on the mesh never answered), that
    /// sibling is dropped; if none is left, the pairs into the mesh refused so
    /// far are formed now, as R1 forms them for a session that never had one.
    pub(crate) async fn settle_sibling(&self, bit: u8) {
        if self.mesh_sibling.load(Ordering::SeqCst) & bit == 0 {
            return;
        }
        let Some(rank) = (*self.carrier_rank).as_ref() else {
            return;
        };
        let made_it = {
            let local_candidates = self.local_candidates.lock().await;
            local_candidates.values().flatten().any(|c| {
                (c.candidate_type() == CandidateType::Host) == (bit == SIBLING_HOST)
                    && rank.local_on_mesh(&**c)
            })
        };
        if made_it {
            return;
        }
        let left = self.mesh_sibling.fetch_and(!bit, Ordering::SeqCst) & !bit;
        log::debug!(
            "[{}]: the gather made no candidate on the mesh for sibling {}; sibling left: {}",
            self.get_name(),
            bit,
            left != 0
        );
        if left != 0 {
            return;
        }
        // Read after the sibling is dropped: a candidate added from here on
        // reads it dropped and forms its own pairs; one added before is here.
        let locals: Vec<_> = {
            let local_candidates = self.local_candidates.lock().await;
            local_candidates.values().flatten().cloned().collect()
        };
        let remotes: Vec<_> = {
            let remote_candidates = self.remote_candidates.lock().await;
            remote_candidates.values().flatten().cloned().collect()
        };
        for local in &locals {
            for remote in &remotes {
                if local.network_type() == remote.network_type()
                    && rank.crosses_into_the_mesh(&**local, &**remote)
                {
                    self.add_pair(Arc::clone(local), Arc::clone(remote)).await;
                }
            }
        }
        self.request_connectivity_check();
    }

    pub(crate) async fn find_pair(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) -> Option<Arc<CandidatePair>> {
        let checklist = self.agent_conn.checklist.lock().await;
        for p in &*checklist {
            if p.local.equal(&**local) && p.remote.equal(&**remote) {
                return Some(p.clone());
            }
        }
        None
    }

    /// Roomler patch (FR-148 P4 step 2, D49 rule 3): does the controlled agent
    /// hold its mesh-route pairs now, neither checking nor answering them? Only
    /// under the route rank where it acts, and only while all of these hold:
    ///
    /// - the bound has not passed since the checks started ([`RouteRank::hold`]);
    /// - no pair is selected (a pair off the mesh ends it: `real-selected`; one
    ///   on the mesh can only be selected once the hold has stopped);
    /// - a relay off the mesh can still come: an allocation on a TURN server
    ///   off the mesh is still running, or a pair off the mesh has not failed.
    ///
    /// Read again at every check and every inbound check, so it stops on the
    /// evidence that nothing off the mesh can come, or on the bound. Until a
    /// pair is selected a pair off the mesh that forms later (a remote relay
    /// trickled late) holds it again; once a pair is selected, or the bound has
    /// passed, it never holds again. When and why it stopped is kept for the
    /// session's opening (`route_hold_so_far`).
    pub(crate) async fn holds_mesh_route(&self) -> bool {
        let Some(rank) = (*self.carrier_rank).as_ref() else {
            return false;
        };
        let Some(route) = rank.route_engaged() else {
            return false;
        };
        if self.is_controlling.load(Ordering::SeqCst) {
            return false;
        }
        let started = {
            let hold = self.route_hold.lock();
            if hold.over {
                return false;
            }
            // Nothing is checked or answered before the checks start.
            let Some(started) = hold.started else {
                return true;
            };
            started
        };
        let bound = started + route.hold;
        if Instant::now() >= bound {
            let mut hold = self.route_hold.lock();
            if hold.stopped.is_none() {
                hold.stopped = Some((bound, RouteHoldExit::Bound));
            }
            hold.over = true;
            return false;
        }
        if let Some(selected) = self.agent_conn.get_selected_pair() {
            // Its selection has ended the hold already; this only repeats it.
            self.hold_ends_at_selection(&selected);
            return false;
        }
        let real_can_come = self.real_turns_pending.load(Ordering::SeqCst) > 0 || {
            let checklist = self.agent_conn.checklist.lock().await;
            checklist.iter().any(|p| {
                p.state.load(Ordering::SeqCst) != CandidatePairState::Failed as u8
                    && !rank.mesh_route(&*p.local, &*p.remote)
            })
        };
        let mut hold = self.route_hold.lock();
        if hold.over {
            return false;
        }
        if real_can_come {
            hold.stopped = None;
        } else if hold.stopped.is_none() {
            hold.stopped = Some((Instant::now(), RouteHoldExit::NothingReal));
        }
        real_can_come
    }

    /// Roomler patch (FR-148 P4 step 2): the selection of `p` ends the hold for
    /// good. While the hold holds, only a pair off the mesh can be selected:
    /// a mesh-route pair is neither checked nor answered, so it cannot be valid.
    fn hold_ends_at_selection(&self, p: &CandidatePair) {
        let Some(rank) = (*self.carrier_rank).as_ref() else {
            return;
        };
        let Some(route) = rank.route_engaged() else {
            return;
        };
        if self.is_controlling.load(Ordering::SeqCst) {
            return;
        }
        let mut hold = self.route_hold.lock();
        let Some(started) = hold.started else {
            return;
        };
        if hold.over {
            return;
        }
        hold.over = true;
        if hold.stopped.is_some() {
            return;
        }
        let (now, bound) = (Instant::now(), started + route.hold);
        if now >= bound {
            hold.stopped = Some((bound, RouteHoldExit::Bound));
        } else if !rank.mesh_route(&*p.local, &*p.remote) {
            hold.stopped = Some((now, RouteHoldExit::RealSelected));
        }
    }

    /// Roomler patch (FR-148 P4 step 2): how long the hold held, from the
    /// checks' start to when it stopped, and why it stopped. Zero and `None`
    /// when it never applied or has not stopped.
    pub(crate) fn route_hold_so_far(&self) -> (Duration, Option<RouteHoldExit>) {
        let hold = self.route_hold.lock();
        match (hold.started, hold.stopped) {
            (Some(started), Some((at, exit))) => (at.saturating_duration_since(started), Some(exit)),
            _ => (Duration::ZERO, None),
        }
    }

    /// Checks if the selected pair is (still) valid.
    /// Note: the caller should hold the agent lock.
    pub(crate) async fn validate_selected_pair(&self) -> bool {
        let (valid, disconnected_time) = {
            let selected_pair = self.agent_conn.selected_pair.load();
            (*selected_pair).as_ref().map_or_else(
                || (false, Duration::from_secs(0)),
                |selected_pair| {
                    let disconnected_time = SystemTime::now()
                        .duration_since(selected_pair.remote.last_received())
                        .unwrap_or_else(|_| Duration::from_secs(0));
                    (true, disconnected_time)
                },
            )
        };

        if valid {
            // Only allow transitions to failed if a.failedTimeout is non-zero
            let mut total_time_to_failure = self.failed_timeout;
            if total_time_to_failure != Duration::from_secs(0) {
                total_time_to_failure += self.disconnected_timeout;
            }

            if total_time_to_failure != Duration::from_secs(0)
                && disconnected_time > total_time_to_failure
            {
                self.update_connection_state(ConnectionState::Failed).await;
            } else if self.disconnected_timeout != Duration::from_secs(0)
                && disconnected_time > self.disconnected_timeout
            {
                self.update_connection_state(ConnectionState::Disconnected)
                    .await;
            } else {
                self.update_connection_state(ConnectionState::Connected)
                    .await;
            }
        }

        valid
    }

    /// Sends STUN Binding Indications to the selected pair.
    /// if no packet has been sent on that pair in the last keepaliveInterval.
    /// Note: the caller should hold the agent lock.
    pub(crate) async fn check_keepalive(&self) {
        let (local, remote) = {
            let selected_pair = self.agent_conn.selected_pair.load();
            (*selected_pair)
                .as_ref()
                .map_or((None, None), |selected_pair| {
                    (
                        Some(selected_pair.local.clone()),
                        Some(selected_pair.remote.clone()),
                    )
                })
        };

        if let (Some(local), Some(remote)) = (local, remote) {
            let last_sent = SystemTime::now()
                .duration_since(local.last_sent())
                .unwrap_or_else(|_| Duration::from_secs(0));

            let last_received = SystemTime::now()
                .duration_since(remote.last_received())
                .unwrap_or_else(|_| Duration::from_secs(0));

            if (self.keepalive_interval != Duration::from_secs(0))
                && ((last_sent > self.keepalive_interval)
                    || (last_received > self.keepalive_interval))
            {
                // we use binding request instead of indication to support refresh consent schemas
                // see https://tools.ietf.org/html/rfc7675
                self.ping_candidate(&local, &remote).await;
            }
        }
    }

    fn request_connectivity_check(&self) {
        let _ = self.force_candidate_contact_tx.try_send(true);
    }

    /// Assumes you are holding the lock (must be execute using a.run).
    pub(crate) async fn add_remote_candidate(&self, c: &Arc<dyn Candidate + Send + Sync>) {
        let network_type = c.network_type();

        {
            let mut remote_candidates = self.remote_candidates.lock().await;
            if let Some(cands) = remote_candidates.get(&network_type) {
                for cand in cands {
                    if cand.equal(&**c) {
                        return;
                    }
                }
            }

            if let Some(cands) = remote_candidates.get_mut(&network_type) {
                cands.push(c.clone());
            } else {
                remote_candidates.insert(network_type, vec![c.clone()]);
            }
        }

        let mut local_cands = vec![];
        {
            let local_candidates = self.local_candidates.lock().await;
            if let Some(cands) = local_candidates.get(&network_type) {
                local_cands.clone_from(cands);
            }
        }

        for cand in local_cands {
            self.add_pair(cand, c.clone()).await;
        }

        self.request_connectivity_check();
    }

    pub(crate) async fn add_candidate(
        self: &Arc<Self>,
        c: &Arc<dyn Candidate + Send + Sync>,
    ) -> Result<()> {
        let initialized_ch = {
            let started_ch_tx = self.started_ch_tx.lock().await;
            (*started_ch_tx).as_ref().map(|tx| tx.subscribe())
        };

        self.start_candidate(c, initialized_ch).await;

        let network_type = c.network_type();
        {
            let mut local_candidates = self.local_candidates.lock().await;
            // VENDOR PATCH (#1740): a candidate whose gathering finished after
            // close() (a late STUN or TURN answer) was started above and kept,
            // and nothing would ever close it: its receive loop held the agent,
            // and the agent every socket. Read under the lock close()'s sweep
            // takes, after close() raised the flag, so a candidate lands either
            // before the sweep (which closes it) or here.
            let closed = *self.closed.borrow();
            // VENDOR PATCH (#3716): a candidate turned away is closed after the
            // lock is let go. A close can wait on the network (a relay's TURN
            // client), and close()'s sweep waited on this lock behind it.
            if closed {
                drop(local_candidates);
                if let Err(err) = c.close().await {
                    log::warn!(
                        "[{}]: Failed to close a candidate gathered after close: {}",
                        self.get_name(),
                        err
                    );
                }
                return Ok(());
            }
            let duplicate = local_candidates
                .get(&network_type)
                .is_some_and(|cands| cands.iter().any(|cand| cand.equal(&**c)));
            if duplicate {
                drop(local_candidates);
                if let Err(err) = c.close().await {
                    log::warn!(
                        "[{}]: Failed to close duplicate candidate: {}",
                        self.get_name(),
                        err
                    );
                }
                //TODO: why return?
                return Ok(());
            }

            if let Some(cands) = local_candidates.get_mut(&network_type) {
                cands.push(c.clone());
            } else {
                local_candidates.insert(network_type, vec![c.clone()]);
            }
        }

        let mut remote_cands = vec![];
        {
            let remote_candidates = self.remote_candidates.lock().await;
            if let Some(cands) = remote_candidates.get(&network_type) {
                remote_cands.clone_from(cands);
            }
        }

        for cand in remote_cands {
            self.add_pair(c.clone(), cand).await;
        }

        self.request_connectivity_check();
        {
            let chan_candidate_tx = self.chan_candidate_tx.lock().await;
            if let Some(tx) = &*chan_candidate_tx {
                let _ = tx.send(Some(c.clone())).await;
            }
        }

        Ok(())
    }

    pub(crate) async fn close(&self) -> Result<()> {
        {
            let mut done_tx = self.done_tx.lock().await;
            if done_tx.is_none() {
                return Err(Error::ErrClosed);
            }
            done_tx.take();
        };
        // VENDOR PATCH (#1740): before the sweep, so a candidate gathered from
        // here on is turned away by add_candidate() instead of outliving us.
        self.closed.send_replace(true);
        self.delete_all_candidates().await;
        {
            let mut started_ch_tx = self.started_ch_tx.lock().await;
            started_ch_tx.take();
        }

        self.agent_conn.buffer.close().await;

        self.update_connection_state(ConnectionState::Closed).await;

        {
            let mut chan_candidate_tx = self.chan_candidate_tx.lock().await;
            chan_candidate_tx.take();
        }
        {
            let mut chan_candidate_pair_tx = self.chan_candidate_pair_tx.lock().await;
            chan_candidate_pair_tx.take();
        }
        {
            let mut chan_state_tx = self.chan_state_tx.lock().await;
            chan_state_tx.take();
        }

        self.agent_conn.done.store(true, Ordering::SeqCst);

        Ok(())
    }

    /// Remove all candidates.
    /// This closes any listening sockets and removes both the local and remote candidate lists.
    ///
    /// This is used for restarts, failures and on close.
    pub(crate) async fn delete_all_candidates(&self) {
        // VENDOR PATCH (#3716): the lists are taken out under their locks and
        // closed after, each local candidate (its socket, its TURN client) on a
        // task of its own. A candidate's close can wait on the network, and the
        // sweep closed them one at a time under `local_candidates`: one slow
        // close held the agent's close, and with it every socket the agent
        // owned. A close still slow after `SLOW_CANDIDATE_CLOSE` says which.
        let local_candidates = std::mem::take(&mut *self.local_candidates.lock().await);
        for c in local_candidates.into_values().flatten() {
            let name = self.get_name().to_owned();
            tokio::spawn(async move {
                let close = c.close();
                tokio::pin!(close);
                let result = tokio::select! {
                    result = &mut close => result,
                    _ = tokio::time::sleep(SLOW_CANDIDATE_CLOSE) => {
                        log::warn!(
                            "[{}]: closing candidate {} is taking over {:?}",
                            name,
                            c,
                            SLOW_CANDIDATE_CLOSE
                        );
                        close.await
                    }
                };
                if let Err(err) = result {
                    log::warn!("[{}]: Failed to close candidate {}: {}", name, c, err);
                }
            });
        }

        let remote_candidates = std::mem::take(&mut *self.remote_candidates.lock().await);
        for c in remote_candidates.into_values().flatten() {
            if let Err(err) = c.close().await {
                log::warn!(
                    "[{}]: Failed to close candidate {}: {}",
                    self.get_name(),
                    c,
                    err
                );
            }
        }
    }

    pub(crate) async fn find_remote_candidate(
        &self,
        network_type: NetworkType,
        addr: SocketAddr,
    ) -> Option<Arc<dyn Candidate + Send + Sync>> {
        let (ip, port) = (addr.ip(), addr.port());

        let remote_candidates = self.remote_candidates.lock().await;
        if let Some(cands) = remote_candidates.get(&network_type) {
            for c in cands {
                if c.address() == ip.to_string() && c.port() == port {
                    return Some(c.clone());
                }
            }
        }
        None
    }

    pub(crate) async fn send_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        log::trace!(
            "[{}]: ping STUN from {} to {}",
            self.get_name(),
            local,
            remote
        );

        self.invalidate_pending_binding_requests(Instant::now())
            .await;
        {
            let mut pending_binding_requests = self.pending_binding_requests.lock().await;
            pending_binding_requests.push(BindingRequest {
                timestamp: Instant::now(),
                transaction_id: m.transaction_id,
                destination: remote.addr(),
                is_use_candidate: m.contains(ATTR_USE_CANDIDATE),
            });
        }

        self.send_stun(m, local, remote).await;
    }

    pub(crate) async fn send_binding_success(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let addr = remote.addr();
        let (ip, port) = (addr.ip(), addr.port());
        let local_pwd = {
            let ufrag_pwd = self.ufrag_pwd.lock().await;
            ufrag_pwd.local_pwd.clone()
        };

        let (out, result) = {
            let mut out = Message::new();
            let result = out.build(&[
                Box::new(m.clone()),
                Box::new(BINDING_SUCCESS),
                Box::new(XorMappedAddress { ip, port }),
                Box::new(MessageIntegrity::new_short_term_integrity(local_pwd)),
                Box::new(FINGERPRINT),
            ]);
            (out, result)
        };

        if let Err(err) = result {
            log::warn!(
                "[{}]: Failed to handle inbound ICE from: {} to: {} error: {}",
                self.get_name(),
                local,
                remote,
                err
            );
        } else {
            self.send_stun(&out, local, remote).await;
        }
    }

    /// Removes pending binding requests that are over `maxBindingRequestTimeout` old Let HTO be the
    /// transaction timeout, which SHOULD be 2*RTT if RTT is known or 500 ms otherwise.
    ///
    /// reference: (IETF ref-8445)[https://tools.ietf.org/html/rfc8445#appendix-B.1].
    pub(crate) async fn invalidate_pending_binding_requests(&self, filter_time: Instant) {
        let mut pending_binding_requests = self.pending_binding_requests.lock().await;
        let initial_size = pending_binding_requests.len();

        let mut temp = vec![];
        for binding_request in pending_binding_requests.drain(..) {
            if filter_time
                .checked_duration_since(binding_request.timestamp)
                .map(|duration| duration < MAX_BINDING_REQUEST_TIMEOUT)
                .unwrap_or(true)
            {
                temp.push(binding_request);
            }
        }

        *pending_binding_requests = temp;
        let bind_requests_removed = initial_size - pending_binding_requests.len();
        if bind_requests_removed > 0 {
            log::trace!(
                "[{}]: Discarded {} binding requests because they expired",
                self.get_name(),
                bind_requests_removed
            );
        }
    }

    /// Assert that the passed `TransactionID` is in our `pendingBindingRequests` and returns the
    /// destination, If the bindingRequest was valid remove it from our pending cache.
    pub(crate) async fn handle_inbound_binding_success(
        &self,
        id: TransactionId,
    ) -> Option<BindingRequest> {
        self.invalidate_pending_binding_requests(Instant::now())
            .await;

        let mut pending_binding_requests = self.pending_binding_requests.lock().await;
        for i in 0..pending_binding_requests.len() {
            if pending_binding_requests[i].transaction_id == id {
                let valid_binding_request = pending_binding_requests.remove(i);
                return Some(valid_binding_request);
            }
        }
        None
    }

    /// Processes STUN traffic from a remote candidate.
    pub(crate) async fn handle_inbound(
        &self,
        m: &mut Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: SocketAddr,
    ) {
        if m.typ.method != METHOD_BINDING
            || !(m.typ.class == CLASS_SUCCESS_RESPONSE
                || m.typ.class == CLASS_REQUEST
                || m.typ.class == CLASS_INDICATION)
        {
            log::trace!(
                "[{}]: unhandled STUN from {} to {} class({}) method({})",
                self.get_name(),
                remote,
                local,
                m.typ.class,
                m.typ.method
            );
            return;
        }

        if self.is_controlling.load(Ordering::SeqCst) {
            if m.contains(ATTR_ICE_CONTROLLING) {
                log::debug!(
                    "[{}]: inbound isControlling && a.isControlling == true",
                    self.get_name(),
                );
                return;
            } else if m.contains(ATTR_USE_CANDIDATE) {
                log::debug!(
                    "[{}]: useCandidate && a.isControlling == true",
                    self.get_name(),
                );
                return;
            }
        } else if m.contains(ATTR_ICE_CONTROLLED) {
            log::debug!(
                "[{}]: inbound isControlled && a.isControlling == false",
                self.get_name(),
            );
            return;
        }

        let mut remote_candidate = self
            .find_remote_candidate(local.network_type(), remote)
            .await;
        if m.typ.class == CLASS_SUCCESS_RESPONSE {
            {
                let ufrag_pwd = self.ufrag_pwd.lock().await;
                if let Err(err) =
                    assert_inbound_message_integrity(m, ufrag_pwd.remote_pwd.as_bytes())
                {
                    log::warn!(
                        "[{}]: discard message from ({}), {}",
                        self.get_name(),
                        remote,
                        err
                    );
                    return;
                }
            }

            if let Some(rc) = &remote_candidate {
                self.handle_success_response(m, local, rc, remote).await;
            } else {
                log::warn!(
                    "[{}]: discard success message from ({}), no such remote",
                    self.get_name(),
                    remote
                );
                return;
            }
        } else if m.typ.class == CLASS_REQUEST {
            {
                let ufrag_pwd = self.ufrag_pwd.lock().await;
                let username =
                    ufrag_pwd.local_ufrag.clone() + ":" + ufrag_pwd.remote_ufrag.as_str();
                if let Err(err) = assert_inbound_username(m, &username) {
                    log::warn!(
                        "[{}]: discard message from ({}), {}",
                        self.get_name(),
                        remote,
                        err
                    );
                    return;
                } else if let Err(err) =
                    assert_inbound_message_integrity(m, ufrag_pwd.local_pwd.as_bytes())
                {
                    log::warn!(
                        "[{}]: discard message from ({}), {}",
                        self.get_name(),
                        remote,
                        err
                    );
                    return;
                }
            }

            if remote_candidate.is_none() {
                let (ip, port, network_type) = (remote.ip(), remote.port(), NetworkType::Udp4);

                let prflx_candidate_config = CandidatePeerReflexiveConfig {
                    base_config: CandidateBaseConfig {
                        network: network_type.to_string(),
                        address: ip.to_string(),
                        port,
                        component: local.component(),
                        ..CandidateBaseConfig::default()
                    },
                    rel_addr: "".to_owned(),
                    rel_port: 0,
                };

                match prflx_candidate_config.new_candidate_peer_reflexive() {
                    Ok(prflx_candidate) => remote_candidate = Some(Arc::new(prflx_candidate)),
                    Err(err) => {
                        log::error!(
                            "[{}]: Failed to create new remote prflx candidate ({})",
                            self.get_name(),
                            err
                        );
                        return;
                    }
                };

                log::debug!(
                    "[{}]: adding a new peer-reflexive candidate: {} ",
                    self.get_name(),
                    remote
                );
                if let Some(rc) = &remote_candidate {
                    self.add_remote_candidate(rc).await;
                }
            }

            log::trace!(
                "[{}]: inbound STUN (Request) from {} to {}",
                self.get_name(),
                remote,
                local
            );

            if let Some(rc) = &remote_candidate {
                self.handle_binding_request(m, local, rc).await;
            }
        }

        if let Some(rc) = remote_candidate {
            rc.seen(false);
        }
    }

    /// Processes non STUN traffic from a remote candidate, and returns true if it is an actual
    /// remote candidate.
    pub(crate) async fn validate_non_stun_traffic(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: SocketAddr,
    ) -> bool {
        self.find_remote_candidate(local.network_type(), remote)
            .await
            .map_or(false, |remote_candidate| {
                remote_candidate.seen(false);
                true
            })
    }

    /// Sets the credentials of the remote agent.
    pub(crate) async fn set_remote_credentials(
        &self,
        remote_ufrag: String,
        remote_pwd: String,
    ) -> Result<()> {
        if remote_ufrag.is_empty() {
            return Err(Error::ErrRemoteUfragEmpty);
        } else if remote_pwd.is_empty() {
            return Err(Error::ErrRemotePwdEmpty);
        }

        let mut ufrag_pwd = self.ufrag_pwd.lock().await;
        ufrag_pwd.remote_ufrag = remote_ufrag;
        ufrag_pwd.remote_pwd = remote_pwd;
        Ok(())
    }

    pub(crate) async fn send_stun(
        &self,
        msg: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if let Err(err) = local.write_to(&msg.raw, &**remote).await {
            log::trace!(
                "[{}]: failed to send STUN message: {}",
                self.get_name(),
                err
            );
        }
    }

    /// Runs the candidate using the provided connection.
    async fn start_candidate(
        self: &Arc<Self>,
        candidate: &Arc<dyn Candidate + Send + Sync>,
        initialized_ch: Option<broadcast::Receiver<()>>,
    ) {
        let (closed_ch_tx, closed_ch_rx) = broadcast::channel(1);
        {
            let closed_ch = candidate.get_closed_ch();
            let mut closed = closed_ch.lock().await;
            *closed = Some(closed_ch_tx);
        }

        let cand = Arc::clone(candidate);
        if let Some(conn) = candidate.get_conn() {
            let conn = Arc::clone(conn);
            let addr = candidate.addr();
            let ai = Arc::clone(self);
            tokio::spawn(async move {
                let _ = ai
                    .recv_loop(cand, closed_ch_rx, initialized_ch, conn, addr)
                    .await;
            });
        } else {
            log::error!("[{}]: Can't start due to conn is_none", self.get_name(),);
        }
    }

    pub(super) fn start_on_connection_state_change_routine(
        self: &Arc<Self>,
        mut chan_state_rx: mpsc::Receiver<ConnectionState>,
        mut chan_candidate_rx: mpsc::Receiver<Option<Arc<dyn Candidate + Send + Sync>>>,
        mut chan_candidate_pair_rx: mpsc::Receiver<()>,
    ) {
        let ai = Arc::clone(self);
        tokio::spawn(async move {
            // CandidatePair and ConnectionState are usually changed at once.
            // Blocking one by the other one causes deadlock.
            while chan_candidate_pair_rx.recv().await.is_some() {
                if let (Some(cb), Some(p)) = (
                    &*ai.on_selected_candidate_pair_change_hdlr.load(),
                    &*ai.agent_conn.selected_pair.load(),
                ) {
                    let mut f = cb.lock().await;
                    f(&p.local, &p.remote).await;
                }
            }
        });

        let ai = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    opt_state = chan_state_rx.recv() => {
                        if let Some(s) = opt_state {
                            if let Some(handler) = &*ai.on_connection_state_change_hdlr.load() {
                                let mut f = handler.lock().await;
                                f(s).await;
                            }
                        } else {
                            while let Some(c) = chan_candidate_rx.recv().await {
                                if let Some(handler) = &*ai.on_candidate_hdlr.load() {
                                    let mut f = handler.lock().await;
                                    f(c).await;
                                }
                            }
                            break;
                        }
                    },
                    opt_cand = chan_candidate_rx.recv() => {
                        if let Some(c) = opt_cand {
                            if let Some(handler) = &*ai.on_candidate_hdlr.load() {
                                let mut f = handler.lock().await;
                                f(c).await;
                            }
                        } else {
                            while let Some(s) = chan_state_rx.recv().await {
                                if let Some(handler) = &*ai.on_connection_state_change_hdlr.load() {
                                    let mut f = handler.lock().await;
                                    f(s).await;
                                }
                            }
                            break;
                        }
                    }
                }
            }
        });
    }

    async fn recv_loop(
        self: &Arc<Self>,
        candidate: Arc<dyn Candidate + Send + Sync>,
        mut closed_ch_rx: broadcast::Receiver<()>,
        initialized_ch: Option<broadcast::Receiver<()>>,
        conn: Arc<dyn util::Conn + Send + Sync>,
        addr: SocketAddr,
    ) -> Result<()> {
        if let Some(mut initialized_ch) = initialized_ch {
            tokio::select! {
                _ = initialized_ch.recv() => {}
                _ = closed_ch_rx.recv() => return Err(Error::ErrClosed),
            }
        }

        let mut buffer = vec![0_u8; RECEIVE_MTU];
        let mut n;
        let mut src_addr;
        loop {
            tokio::select! {
               result = conn.recv_from(&mut buffer) => {
                   match result {
                       Ok((num, src)) => {
                            n = num;
                            src_addr = src;
                       }
                       Err(err) => return Err(Error::Other(err.to_string())),
                   }
               },
                _  = closed_ch_rx.recv() => return Err(Error::ErrClosed),
            }

            self.handle_inbound_candidate_msg(&candidate, &buffer[..n], src_addr, addr)
                .await;
        }
    }

    async fn handle_inbound_candidate_msg(
        self: &Arc<Self>,
        c: &Arc<dyn Candidate + Send + Sync>,
        buf: &[u8],
        src_addr: SocketAddr,
        addr: SocketAddr,
    ) {
        if stun::message::is_message(buf) {
            let mut m = Message {
                raw: vec![],
                ..Message::default()
            };
            // Explicitly copy raw buffer so Message can own the memory.
            m.raw.extend_from_slice(buf);

            if let Err(err) = m.decode() {
                log::warn!(
                    "[{}]: Failed to handle decode ICE from {} to {}: {}",
                    self.get_name(),
                    addr,
                    src_addr,
                    err
                );
            } else {
                self.handle_inbound(&mut m, c, src_addr).await;
            }
        } else if !self.validate_non_stun_traffic(c, src_addr).await {
            log::warn!(
                "[{}]: Discarded message, not a valid remote candidate",
                self.get_name(),
                //c.addr().await //from {}
            );
        } else if let Err(err) = self.agent_conn.buffer.write(buf).await {
            // NOTE This will return packetio.ErrFull if the buffer ever manages to fill up.
            log::warn!("[{}]: failed to write packet: {}", self.get_name(), err);
        }
    }

    pub(crate) fn get_name(&self) -> &str {
        if self.is_controlling.load(Ordering::SeqCst) {
            "controlling"
        } else {
            "controlled"
        }
    }
}
