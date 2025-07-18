// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

mod peers;

use std::{num::NonZeroUsize, time::Duration};

use either::Either;
use fnv::FnvHashMap;
use libp2p_core::Multiaddr;
use libp2p_identity::PeerId;
use peers::{
    closest::{disjoint::ClosestDisjointPeersIter, ClosestPeersIter, ClosestPeersIterConfig},
    fixed::FixedPeersIter,
    PeersIterState,
};
use smallvec::SmallVec;
use web_time::Instant;

use crate::{
    behaviour::PeerInfo,
    handler::HandlerIn,
    kbucket::{Key, KeyBytes},
    QueryInfo, ALPHA_VALUE, K_VALUE,
};

/// A `QueryPool` provides an aggregate state machine for driving `Query`s to completion.
///
/// Internally, a `Query` is in turn driven by an underlying `QueryPeerIter`
/// that determines the peer selection strategy, i.e. the order in which the
/// peers involved in the query should be contacted.
pub(crate) struct QueryPool {
    next_id: usize,
    config: QueryConfig,
    queries: FnvHashMap<QueryId, Query>,
}

/// The observable states emitted by [`QueryPool::poll`].
pub(crate) enum QueryPoolState<'a> {
    /// The pool is idle, i.e. there are no queries to process.
    Idle,
    /// At least one query is waiting for results. `Some(request)` indicates
    /// that a new request is now being waited on.
    Waiting(Option<(&'a mut Query, PeerId)>),
    /// A query has finished.
    Finished(Query),
    /// A query has timed out.
    Timeout(Query),
}

impl QueryPool {
    /// Creates a new `QueryPool` with the given configuration.
    pub(crate) fn new(config: QueryConfig) -> Self {
        QueryPool {
            next_id: 0,
            config,
            queries: Default::default(),
        }
    }

    /// Gets a reference to the `QueryConfig` used by the pool.
    pub(crate) fn config(&self) -> &QueryConfig {
        &self.config
    }

    /// Returns an iterator over the queries in the pool.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Query> {
        self.queries.values()
    }

    /// Gets the current size of the pool, i.e. the number of running queries.
    pub(crate) fn size(&self) -> usize {
        self.queries.len()
    }

    /// Returns an iterator that allows modifying each query in the pool.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Query> {
        self.queries.values_mut()
    }

    /// Adds a query to the pool that contacts a fixed set of peers.
    pub(crate) fn add_fixed<I>(&mut self, peers: I, info: QueryInfo) -> QueryId
    where
        I: IntoIterator<Item = PeerId>,
    {
        let id = self.next_query_id();
        self.continue_fixed(id, peers, info);
        id
    }

    /// Continues an earlier query with a fixed set of peers, reusing
    /// the given query ID, which must be from a query that finished
    /// earlier.
    pub(crate) fn continue_fixed<I>(&mut self, id: QueryId, peers: I, info: QueryInfo)
    where
        I: IntoIterator<Item = PeerId>,
    {
        assert!(!self.queries.contains_key(&id));
        let parallelism = self.config.replication_factor;
        let peer_iter = QueryPeerIter::Fixed(FixedPeersIter::new(peers, parallelism));
        let query = Query::new(id, peer_iter, info);
        self.queries.insert(id, query);
    }

    /// Adds a query to the pool that iterates towards the closest peers to the target.
    pub(crate) fn add_iter_closest<T, I>(&mut self, target: T, peers: I, info: QueryInfo) -> QueryId
    where
        T: Into<KeyBytes> + Clone,
        I: IntoIterator<Item = Key<PeerId>>,
    {
        let id = self.next_query_id();
        self.continue_iter_closest(id, target, peers, info);
        id
    }

    /// Adds a query to the pool that iterates towards the closest peers to the target.
    pub(crate) fn continue_iter_closest<T, I>(
        &mut self,
        id: QueryId,
        target: T,
        peers: I,
        info: QueryInfo,
    ) where
        T: Into<KeyBytes> + Clone,
        I: IntoIterator<Item = Key<PeerId>>,
    {
        let num_results = match info {
            QueryInfo::GetClosestPeers {
                num_results: val, ..
            } => val,
            QueryInfo::Bootstrap { .. } => K_VALUE,
            _ => self.config.replication_factor,
        };

        let cfg = ClosestPeersIterConfig {
            num_results,
            parallelism: self.config.parallelism,
            ..ClosestPeersIterConfig::default()
        };

        let peer_iter = if self.config.disjoint_query_paths {
            QueryPeerIter::ClosestDisjoint(ClosestDisjointPeersIter::with_config(
                cfg, target, peers,
            ))
        } else {
            QueryPeerIter::Closest(ClosestPeersIter::with_config(cfg, target, peers))
        };

        let query = Query::new(id, peer_iter, info);
        self.queries.insert(id, query);
    }

    fn next_query_id(&mut self) -> QueryId {
        let id = QueryId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Returns a reference to a query with the given ID, if it is in the pool.
    pub(crate) fn get(&self, id: &QueryId) -> Option<&Query> {
        self.queries.get(id)
    }

    /// Returns a mutablereference to a query with the given ID, if it is in the pool.
    pub(crate) fn get_mut(&mut self, id: &QueryId) -> Option<&mut Query> {
        self.queries.get_mut(id)
    }

    /// Polls the pool to advance the queries.
    pub(crate) fn poll(&mut self, now: Instant) -> QueryPoolState<'_> {
        let mut finished = None;
        let mut timeout = None;
        let mut waiting = None;

        for (&query_id, query) in self.queries.iter_mut() {
            query.stats.start = query.stats.start.or(Some(now));
            match query.next(now) {
                PeersIterState::Finished => {
                    finished = Some(query_id);
                    break;
                }
                PeersIterState::Waiting(Some(peer_id)) => {
                    let peer = peer_id.into_owned();
                    waiting = Some((query_id, peer));
                    break;
                }
                PeersIterState::Waiting(None) | PeersIterState::WaitingAtCapacity => {
                    let elapsed = now - query.stats.start.unwrap_or(now);
                    if elapsed >= self.config.timeout {
                        timeout = Some(query_id);
                        break;
                    }
                }
            }
        }

        if let Some((query_id, peer_id)) = waiting {
            let query = self.queries.get_mut(&query_id).expect("s.a.");
            return QueryPoolState::Waiting(Some((query, peer_id)));
        }

        if let Some(query_id) = finished {
            let mut query = self.queries.remove(&query_id).expect("s.a.");
            query.stats.end = Some(now);
            return QueryPoolState::Finished(query);
        }

        if let Some(query_id) = timeout {
            let mut query = self.queries.remove(&query_id).expect("s.a.");
            query.stats.end = Some(now);
            return QueryPoolState::Timeout(query);
        }

        if self.queries.is_empty() {
            QueryPoolState::Idle
        } else {
            QueryPoolState::Waiting(None)
        }
    }
}

/// Unique identifier for an active query.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct QueryId(usize);

impl std::fmt::Display for QueryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The configuration for queries in a `QueryPool`.
#[derive(Debug, Clone)]
pub(crate) struct QueryConfig {
    /// Timeout of a single query.
    ///
    /// See [`crate::behaviour::Config::set_query_timeout`] for details.
    pub(crate) timeout: Duration,
    /// The replication factor to use.
    ///
    /// See [`crate::behaviour::Config::set_replication_factor`] for details.
    pub(crate) replication_factor: NonZeroUsize,
    /// Allowed level of parallelism for iterative queries.
    ///
    /// See [`crate::behaviour::Config::set_parallelism`] for details.
    pub(crate) parallelism: NonZeroUsize,
    /// Whether to use disjoint paths on iterative lookups.
    ///
    /// See [`crate::behaviour::Config::disjoint_query_paths`] for details.
    pub(crate) disjoint_query_paths: bool,
}

impl Default for QueryConfig {
    fn default() -> Self {
        QueryConfig {
            timeout: Duration::from_secs(60),
            replication_factor: NonZeroUsize::new(K_VALUE.get()).expect("K_VALUE > 0"),
            parallelism: ALPHA_VALUE,
            disjoint_query_paths: false,
        }
    }
}

/// A query in a `QueryPool`.
pub(crate) struct Query {
    /// The unique ID of the query.
    id: QueryId,
    /// The peer iterator that drives the query state.
    pub(crate) peers: QueryPeers,
    /// Execution statistics of the query.
    pub(crate) stats: QueryStats,
    /// The query-specific state.
    pub(crate) info: QueryInfo,
    /// A map of pending requests to peers.
    ///
    /// A request is pending if the targeted peer is not currently connected
    /// and these requests are sent as soon as a connection to the peer is established.
    pub(crate) pending_rpcs: SmallVec<[(PeerId, HandlerIn); K_VALUE.get()]>,
}

/// The peer iterator that drives the query state,
pub(crate) struct QueryPeers {
    /// Addresses of peers discovered during a query.
    pub(crate) addresses: FnvHashMap<PeerId, SmallVec<[Multiaddr; 8]>>,
    /// The peer iterator that drives the query state.
    peer_iter: QueryPeerIter,
}

impl QueryPeers {
    /// Consumes the peers iterator, producing a final `Iterator` over the discovered `PeerId`s.
    pub(crate) fn into_peerids_iter(self) -> impl Iterator<Item = PeerId> {
        match self.peer_iter {
            QueryPeerIter::Closest(iter) => Either::Left(Either::Left(iter.into_result())),
            QueryPeerIter::ClosestDisjoint(iter) => Either::Left(Either::Right(iter.into_result())),
            QueryPeerIter::Fixed(iter) => Either::Right(iter.into_result()),
        }
    }

    /// Consumes the peers iterator, producing a final `Iterator` over the discovered `PeerId`s
    /// with their matching `Multiaddr`s.
    pub(crate) fn into_peerinfos_iter(mut self) -> impl Iterator<Item = PeerInfo> {
        match self.peer_iter {
            QueryPeerIter::Closest(iter) => Either::Left(Either::Left(iter.into_result())),
            QueryPeerIter::ClosestDisjoint(iter) => Either::Left(Either::Right(iter.into_result())),
            QueryPeerIter::Fixed(iter) => Either::Right(iter.into_result()),
        }
        .map(move |peer_id| {
            let addrs = self.addresses.remove(&peer_id).unwrap_or_default().to_vec();
            PeerInfo { peer_id, addrs }
        })
    }
}

/// The peer selection strategies that can be used by queries.
enum QueryPeerIter {
    Closest(ClosestPeersIter),
    ClosestDisjoint(ClosestDisjointPeersIter),
    Fixed(FixedPeersIter),
}

impl Query {
    /// Creates a new query without starting it.
    fn new(id: QueryId, peer_iter: QueryPeerIter, info: QueryInfo) -> Self {
        Query {
            id,
            info,
            peers: QueryPeers {
                addresses: Default::default(),
                peer_iter,
            },
            pending_rpcs: SmallVec::default(),
            stats: QueryStats::empty(),
        }
    }

    /// Gets the unique ID of the query.
    pub(crate) fn id(&self) -> QueryId {
        self.id
    }

    /// Gets the current execution statistics of the query.
    pub(crate) fn stats(&self) -> &QueryStats {
        &self.stats
    }

    /// Informs the query that the attempt to contact `peer` failed.
    pub(crate) fn on_failure(&mut self, peer: &PeerId) {
        let updated = match &mut self.peers.peer_iter {
            QueryPeerIter::Closest(iter) => iter.on_failure(peer),
            QueryPeerIter::ClosestDisjoint(iter) => iter.on_failure(peer),
            QueryPeerIter::Fixed(iter) => iter.on_failure(peer),
        };
        if updated {
            self.stats.failure += 1;
        }
    }

    /// Informs the query that the attempt to contact `peer` succeeded,
    /// possibly resulting in new peers that should be incorporated into
    /// the query, if applicable.
    pub(crate) fn on_success<I>(&mut self, peer: &PeerId, new_peers: I)
    where
        I: IntoIterator<Item = PeerId>,
    {
        let updated = match &mut self.peers.peer_iter {
            QueryPeerIter::Closest(iter) => iter.on_success(peer, new_peers),
            QueryPeerIter::ClosestDisjoint(iter) => iter.on_success(peer, new_peers),
            QueryPeerIter::Fixed(iter) => iter.on_success(peer),
        };
        if updated {
            self.stats.success += 1;
        }
    }

    /// Advances the state of the underlying peer iterator.
    fn next(&mut self, now: Instant) -> PeersIterState<'_> {
        let state = match &mut self.peers.peer_iter {
            QueryPeerIter::Closest(iter) => iter.next(now),
            QueryPeerIter::ClosestDisjoint(iter) => iter.next(now),
            QueryPeerIter::Fixed(iter) => iter.next(),
        };

        if let PeersIterState::Waiting(Some(_)) = state {
            self.stats.requests += 1;
        }

        state
    }

    /// Tries to (gracefully) finish the query prematurely, providing the peers
    /// that are no longer of interest for further progress of the query.
    ///
    /// A query may require that in order to finish gracefully a certain subset
    /// of peers must be contacted. E.g. in the case of disjoint query paths a
    /// query may only finish gracefully if every path contacted a peer whose
    /// response permits termination of the query. The given peers are those for
    /// which this is considered to be the case, i.e. for which a termination
    /// condition is satisfied.
    ///
    /// Returns `true` if the query did indeed finish, `false` otherwise. In the
    /// latter case, a new attempt at finishing the query may be made with new
    /// `peers`.
    ///
    /// A finished query immediately stops yielding new peers to contact and
    /// will be reported by [`QueryPool::poll`] via
    /// [`QueryPoolState::Finished`].
    pub(crate) fn try_finish<'a, I>(&mut self, peers: I) -> bool
    where
        I: IntoIterator<Item = &'a PeerId>,
    {
        match &mut self.peers.peer_iter {
            QueryPeerIter::Closest(iter) => {
                iter.finish();
                true
            }
            QueryPeerIter::ClosestDisjoint(iter) => iter.finish_paths(peers),
            QueryPeerIter::Fixed(iter) => {
                iter.finish();
                true
            }
        }
    }

    /// Finishes the query prematurely.
    ///
    /// A finished query immediately stops yielding new peers to contact and will be
    /// reported by [`QueryPool::poll`] via [`QueryPoolState::Finished`].
    pub(crate) fn finish(&mut self) {
        match &mut self.peers.peer_iter {
            QueryPeerIter::Closest(iter) => iter.finish(),
            QueryPeerIter::ClosestDisjoint(iter) => iter.finish(),
            QueryPeerIter::Fixed(iter) => iter.finish(),
        }
    }

    /// Checks whether the query has finished.
    ///
    /// A finished query is eventually reported by `QueryPool::next()` and
    /// removed from the pool.
    pub(crate) fn is_finished(&self) -> bool {
        match &self.peers.peer_iter {
            QueryPeerIter::Closest(iter) => iter.is_finished(),
            QueryPeerIter::ClosestDisjoint(iter) => iter.is_finished(),
            QueryPeerIter::Fixed(iter) => iter.is_finished(),
        }
    }
}

/// Execution statistics of a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryStats {
    requests: u32,
    success: u32,
    failure: u32,
    start: Option<Instant>,
    end: Option<Instant>,
}

impl QueryStats {
    pub fn empty() -> Self {
        QueryStats {
            requests: 0,
            success: 0,
            failure: 0,
            start: None,
            end: None,
        }
    }

    /// Gets the total number of requests initiated by the query.
    pub fn num_requests(&self) -> u32 {
        self.requests
    }

    /// Gets the number of successful requests.
    pub fn num_successes(&self) -> u32 {
        self.success
    }

    /// Gets the number of failed requests.
    pub fn num_failures(&self) -> u32 {
        self.failure
    }

    /// Gets the number of pending requests.
    ///
    /// > **Note**: A query can finish while still having pending
    /// > requests, if the termination conditions are already met.
    pub fn num_pending(&self) -> u32 {
        self.requests - (self.success + self.failure)
    }

    /// Gets the duration of the query.
    ///
    /// If the query has not yet finished, the duration is measured from the
    /// start of the query to the current instant.
    ///
    /// If the query did not yet start (i.e. yield the first peer to contact),
    /// `None` is returned.
    pub fn duration(&self) -> Option<Duration> {
        if let Some(s) = self.start {
            if let Some(e) = self.end {
                Some(e - s)
            } else {
                Some(Instant::now() - s)
            }
        } else {
            None
        }
    }

    /// Merges these stats with the given stats of another query,
    /// e.g. to accumulate statistics from a multi-phase query.
    ///
    /// Counters are merged cumulatively while the instants for
    /// start and end of the queries are taken as the minimum and
    /// maximum, respectively.
    pub fn merge(self, other: QueryStats) -> Self {
        QueryStats {
            requests: self.requests + other.requests,
            success: self.success + other.success,
            failure: self.failure + other.failure,
            start: match (self.start, other.start) {
                (Some(a), Some(b)) => Some(std::cmp::min(a, b)),
                (a, b) => a.or(b),
            },
            end: std::cmp::max(self.end, other.end),
        }
    }
}
