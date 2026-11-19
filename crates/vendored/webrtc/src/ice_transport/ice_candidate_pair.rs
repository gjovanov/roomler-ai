use std::fmt;

use ice::url::{ProtoType, SchemeType};

use crate::ice_transport::ice_candidate::*;

/// ICECandidatePair represents an ICE Candidate pair
///
/// ## Specifications
///
/// * [MDN]
///
/// [MDN]: https://developer.mozilla.org/en-US/docs/Web/API/RTCIceCandidatePair
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RTCIceCandidatePair {
    stats_id: String,
    local: RTCIceCandidate,
    remote: RTCIceCandidate,
    /// Roomler patch (FR-148 P0): see [`Self::local_relay_transport`].
    local_relay: Option<(ProtoType, SchemeType)>,
}

impl fmt::Display for RTCIceCandidatePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(local) {} <-> (remote) {}", self.local, self.remote)
    }
}

impl RTCIceCandidatePair {
    fn stats_id(local_id: &str, remote_id: &str) -> String {
        format!("{local_id}-{remote_id}")
    }

    /// returns an initialized ICECandidatePair
    /// for the given pair of ICECandidate instances
    pub fn new(local: RTCIceCandidate, remote: RTCIceCandidate) -> Self {
        let stats_id = Self::stats_id(&local.stats_id, &remote.stats_id);
        RTCIceCandidatePair {
            stats_id,
            local,
            remote,
            local_relay: None,
        }
    }

    /// Roomler patch (FR-148 P0): the pair, with what carries its local
    /// candidate's TURN allocation, as the ICE agent's candidate says it
    /// (`Candidate::relay_transport`, a patch of the vendored webrtc-ice).
    pub(crate) fn with_local_relay(mut self, relay: Option<(ProtoType, SchemeType)>) -> Self {
        self.local_relay = relay;
        self
    }

    /// Roomler patch (FR-148 P0): the transport and the scheme of the TURN URL
    /// the local candidate's allocation was made over. `None` when the local
    /// candidate is not a relay, when its gather did not say, and on a pair
    /// made with [`Self::new`] rather than read from the ICE agent.
    pub fn local_relay_transport(&self) -> Option<(ProtoType, SchemeType)> {
        self.local_relay
    }

    /// The local candidate of the selected pair. Exposed (roomler patch) so
    /// the agent can inspect the negotiated path's candidate `typ`/`protocol`
    /// at runtime (per-session TURN-relay detection for adaptive bitrate).
    pub fn local(&self) -> &RTCIceCandidate {
        &self.local
    }

    /// The remote candidate of the selected pair. See [`Self::local`].
    pub fn remote(&self) -> &RTCIceCandidate {
        &self.remote
    }
}
