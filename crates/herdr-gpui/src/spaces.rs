//! Spaces in attention order: one list across every machine, those that need
//! you first, then the ones you touched most recently, so spaces you leave
//! alone drift down. The order is
//! this client's presentation only. The daemon's own order, which every
//! client shares and dragging changes, is untouched, and status still comes
//! from the daemon's snapshot.

mod jev;
mod order;
mod pins;
mod recency;
mod slides;

pub(crate) use jev::{Candidate, MAX_SPACES as JEV_MAX_SPACES, judge};
pub(crate) use order::{Entry, Unit, rank, units, urgency};
pub(crate) use pins::Pins;
pub(crate) use recency::{Recency, now_millis};
pub(crate) use slides::Slides;

use serde::{Deserialize, Serialize};

/// A space's key in the ranked list, which spans machines.
pub(crate) fn row_key(endpoint: &str, id: &str) -> String {
    format!("{endpoint}\n{id}")
}

/// Endpoint IDs and daemon workspace IDs are short; anything longer is not ours.
const MAX_ID_BYTES: usize = 256;

/// One space on one endpoint: workspace IDs are unique only within a daemon.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct SpaceKey {
    pub endpoint: String,
    pub id: String,
}

impl SpaceKey {
    pub(crate) fn new(endpoint: &str, id: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            id: id.to_owned(),
        }
    }

    /// Whether a key read back from disk could be one of ours.
    pub(crate) fn valid(&self) -> bool {
        [&self.endpoint, &self.id]
            .iter()
            .all(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
    }
}
