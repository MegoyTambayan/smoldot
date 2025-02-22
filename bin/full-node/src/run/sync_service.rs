// Smoldot
// Copyright (C) 2019-2021  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Background synchronization service.
//!
//! The [`SyncService`] manages a background task dedicated to synchronizing the chain with the
//! network.
//! Importantly, its design is oriented towards the particular use case of the full node.

// TODO: doc
// TODO: re-review this once finished

use crate::run::network_service;

use core::{convert::TryFrom as _, num::NonZeroU32, pin::Pin};
use futures::{channel::mpsc, lock::Mutex, prelude::*};
use smoldot::{
    database::full_sqlite,
    executor, header,
    informant::HashDisplay,
    libp2p,
    network::{self, protocol::BlockData, service::BlocksRequestError},
    sync::{all, optimistic},
};
use std::{collections::BTreeMap, num::NonZeroU64, sync::Arc, time::SystemTime};
use tracing::Instrument as _;

/// Configuration for a [`SyncService`].
pub struct Config {
    /// Closure that spawns background tasks.
    pub tasks_executor: Box<dyn FnMut(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Database to use to read and write information about the chain.
    pub database: Arc<full_sqlite::SqliteFullDatabase>,

    /// Access to the network, and index of the chain to sync from the point of view of the
    /// network service.
    pub network_service: (Arc<network_service::NetworkService>, usize),

    /// Receiver for events coming from the network, as returned by
    /// [`network_service::NetworkService::new`].
    pub network_events_receiver: mpsc::Receiver<network_service::Event>,
}

/// Identifier for a blocks request to be performed.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct BlocksRequestId(usize);

/// Summary of the state of the [`SyncService`].
#[derive(Debug, Clone)]
pub struct SyncState {
    pub best_block_number: u64,
    pub best_block_hash: [u8; 32],
    pub finalized_block_number: u64,
    pub finalized_block_hash: [u8; 32],
}

/// Background task that verifies blocks and emits requests.
pub struct SyncService {
    /// State kept up-to-date with the background task.
    sync_state: Arc<Mutex<SyncState>>,
}

impl SyncService {
    /// Initializes the [`SyncService`] with the given configuration.
    #[tracing::instrument(skip(config))]
    pub async fn new(mut config: Config) -> Arc<Self> {
        let (to_database, messages_rx) = mpsc::channel(4);

        let finalized_block_hash = config.database.finalized_block_hash().unwrap();
        let best_block_hash = config.database.best_block_hash().unwrap();

        let sync_state = Arc::new(Mutex::new(SyncState {
            best_block_hash,
            best_block_number: header::decode(
                &config
                    .database
                    .block_scale_encoded_header(&best_block_hash)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
            .number,
            finalized_block_hash,
            finalized_block_number: header::decode(
                &config
                    .database
                    .block_scale_encoded_header(&finalized_block_hash)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
            .number,
        }));

        let background_sync = {
            let finalized_block_storage: BTreeMap<Vec<u8>, Vec<u8>> = config
                .database
                .finalized_block_storage_top_trie(&config.database.finalized_block_hash().unwrap())
                .unwrap();

            let sync = all::AllSync::new(all::Config {
                chain_information: config
                    .database
                    .to_chain_information(&config.database.finalized_block_hash().unwrap())
                    .unwrap(),
                sources_capacity: 32,
                blocks_capacity: {
                    // This is the maximum number of blocks between two consecutive justifications.
                    1024
                },
                max_disjoint_headers: 1024,
                max_requests_per_block: NonZeroU32::new(3).unwrap(),
                download_ahead_blocks: {
                    // Assuming a verification speed of 1k blocks/sec and a 95% latency of one second,
                    // the number of blocks to download ahead of time in order to not block is 1000.
                    NonZeroU32::new(1024).unwrap()
                },
                full: Some(all::ConfigFull {
                    finalized_runtime: {
                        // Builds the runtime of the finalized block.
                        // Assumed to always be valid, otherwise the block wouldn't have been saved in the
                        // database, hence the large number of unwraps here.
                        let module = finalized_block_storage.get(&b":code"[..]).unwrap();
                        let heap_pages = executor::storage_heap_pages_to_value(
                            finalized_block_storage
                                .get(&b":heappages"[..])
                                .map(|v| &v[..]),
                        )
                        .unwrap();
                        executor::host::HostVmPrototype::new(
                            module,
                            heap_pages,
                            executor::vm::ExecHint::CompileAheadOfTime, // TODO: probably should be decided by the optimisticsync
                        )
                        .unwrap()
                    },
                }),
            });

            SyncBackground {
                sync,
                finalized_block_storage,
                sync_state: sync_state.clone(),
                network_service: config.network_service.0,
                network_chain_index: config.network_service.1,
                from_network_service: config.network_events_receiver,
                to_database,
                peers_source_id_map: Default::default(),
                block_requests_finished: stream::FuturesUnordered::new(),
            }
        };

        (config.tasks_executor)(Box::pin(background_sync.run()));

        (config.tasks_executor)(Box::pin(
            start_database_write(config.database, messages_rx).instrument(
                tracing::debug_span!(parent: None, "database-write", root = ?finalized_block_hash), // TDOO: better display
            ),
        ));

        Arc::new(SyncService { sync_state })
    }

    /// Returns a summary of the state of the service.
    ///
    /// > **Important**: This doesn't represent the content of the database.
    // TODO: maybe remove this in favour of the database; seems like a better idea
    #[tracing::instrument(skip(self))]
    pub async fn sync_state(&self) -> SyncState {
        self.sync_state.lock().await.clone()
    }
}

enum ToDatabase {
    FinalizedBlocks(Vec<optimistic::Block<()>>),
}

struct SyncBackground {
    sync: all::AllSync<future::AbortHandle, libp2p::PeerId, ()>,

    /// Holds, in parallel of the database, the storage of the latest finalized block.
    /// At the time of writing, this state is stable around ~3MiB for Polkadot, meaning that it is
    /// completely acceptable to hold it entirely in memory.
    // While reading the storage from the database is an option, doing so considerably slows down
    /// the verification, and also makes it impossible to insert blocks in the database in
    /// parallel of this verification.
    finalized_block_storage: BTreeMap<Vec<u8>, Vec<u8>>,

    sync_state: Arc<Mutex<SyncState>>,
    network_service: Arc<network_service::NetworkService>,
    network_chain_index: usize,
    from_network_service: mpsc::Receiver<network_service::Event>,
    to_database: mpsc::Sender<ToDatabase>,

    peers_source_id_map: hashbrown::HashMap<libp2p::PeerId, all::SourceId, fnv::FnvBuildHasher>,
    block_requests_finished: stream::FuturesUnordered<
        future::BoxFuture<
            'static,
            (
                all::RequestId,
                Result<Result<Vec<BlockData>, BlocksRequestError>, future::Aborted>,
            ),
        >,
    >,
}

impl SyncBackground {
    // TODO: handle obsolete requests
    async fn start_requests(&mut self) {
        loop {
            // `desired_requests()` returns, in decreasing order of priority, the requests
            // that should be started in order for the syncing to proceed. We simply pick the
            // first request, but enforce one ongoing request per source.
            let (source_id, _, mut request_info) =
                match self.sync.desired_requests().find(|(source_id, _, _)| {
                    self.sync.source_num_ongoing_requests(*source_id) == 0
                }) {
                    Some(v) => v,
                    None => break,
                };

            // Before notifying the syncing of the request, clamp the number of blocks to the
            // number of blocks we expect to receive.
            request_info.num_blocks_clamp(NonZeroU64::new(64).unwrap());

            match request_info {
                all::RequestDetail::BlocksRequest {
                    first_block_hash,
                    first_block_height,
                    ascending,
                    num_blocks,
                    request_headers,
                    request_bodies,
                    request_justification,
                } => {
                    let peer_id = self.sync.source_user_data_mut(source_id).clone();

                    let request = self.network_service.clone().blocks_request(
                        peer_id,
                        self.network_chain_index,
                        network::protocol::BlocksRequestConfig {
                            start: if let Some(first_block_hash) = first_block_hash {
                                network::protocol::BlocksRequestConfigStart::Hash(first_block_hash)
                            } else {
                                network::protocol::BlocksRequestConfigStart::Number(
                                    NonZeroU64::new(first_block_height).unwrap(), // TODO: unwrap?
                                )
                            },
                            desired_count: NonZeroU32::new(
                                u32::try_from(num_blocks.get()).unwrap_or(u32::max_value()),
                            )
                            .unwrap(),
                            direction: if ascending {
                                network::protocol::BlocksRequestDirection::Ascending
                            } else {
                                network::protocol::BlocksRequestDirection::Descending
                            },
                            fields: network::protocol::BlocksRequestFields {
                                header: request_headers,
                                body: request_bodies,
                                justification: request_justification,
                            },
                        },
                    );

                    let (request, abort) = future::abortable(request);
                    let request_id = self
                        .sync
                        .add_request(source_id, request_info.clone(), abort);

                    self.block_requests_finished
                        .push(request.map(move |r| (request_id, r)).boxed());
                }
                all::RequestDetail::GrandpaWarpSync { .. }
                | all::RequestDetail::StorageGet { .. } => {
                    // Not used in "full" mode.
                    unreachable!()
                }
            }
        }
    }

    async fn process_blocks(mut self) -> Self {
        // The sync state machine can be in a few various states. At the time of writing:
        // idle, verifying header, verifying block, verifying grandpa warp sync proof,
        // verifying storage proof.
        // If the state is one of the "verifying" states, perform the actual verification and
        // loop again until the sync is in an idle state.
        loop {
            let unix_time = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();

            match self.sync.process_one() {
                all::ProcessOne::AllSync(idle) => {
                    self.sync = idle;
                    break;
                }
                all::ProcessOne::VerifyWarpSyncFragment(_) => unreachable!(),
                all::ProcessOne::VerifyBodyHeader(verify) => {
                    let hash_to_verify = verify.hash();
                    let height_to_verify = verify.height();

                    let span = tracing::debug_span!(
                        "block-verification",
                        hash_to_verify = %HashDisplay(&hash_to_verify), height = %height_to_verify,
                        outcome = tracing::field::Empty, is_new_best = tracing::field::Empty,
                        error = tracing::field::Empty,
                    );
                    let _enter = span.enter();

                    let mut verify = verify.start(unix_time, ());
                    loop {
                        match verify {
                            all::BlockVerification::Error {
                                sync: sync_out,
                                error,
                                ..
                            } => {
                                // Print a separate warning because it is important for the user
                                // to be aware of the verification failure.
                                // `%error` is last because it's quite big.
                                tracing::warn!(
                                    parent: &span, hash = %HashDisplay(&hash_to_verify),
                                    height = %height_to_verify, %error,
                                    "failed-block-verification"
                                );
                                span.record("outcome", &"failure");
                                span.record("error", &tracing::field::display(error));
                                self.sync = sync_out;
                                break;
                            }
                            all::BlockVerification::Finalized {
                                sync: sync_out,
                                finalized_blocks,
                            } => {
                                span.record("outcome", &"success");
                                span.record("is_new_best", &true);

                                let fut = self.network_service.set_local_best_block(
                                    self.network_chain_index,
                                    sync_out.best_block_hash(),
                                    sync_out.best_block_number(),
                                );
                                fut.await;

                                // Processing has made a step forward.
                                // There is nothing to do, but this is used to update the
                                // best block shown on the informant.
                                let mut lock = self.sync_state.lock().await;
                                lock.best_block_hash = sync_out.best_block_hash();
                                lock.best_block_number = sync_out.best_block_number();
                                drop(lock);

                                if let Some(last_finalized) = finalized_blocks.last() {
                                    let mut lock = self.sync_state.lock().await;
                                    lock.finalized_block_hash = last_finalized.header.hash();
                                    lock.finalized_block_number = last_finalized.header.number;
                                }

                                // TODO: maybe write in a separate task? but then we can't access the finalized storage immediately after?
                                for block in &finalized_blocks {
                                    for (key, value) in &block.storage_top_trie_changes {
                                        if let Some(value) = value {
                                            self.finalized_block_storage
                                                .insert(key.clone(), value.clone());
                                        } else {
                                            let _was_there =
                                                self.finalized_block_storage.remove(key);
                                            // TODO: if a block inserts a new value, then removes it in the next block, the key will remain in `finalized_block_storage`; either solve this or document this
                                            // assert!(_was_there.is_some());
                                        }
                                    }
                                }

                                self.to_database
                                    .send(ToDatabase::FinalizedBlocks(finalized_blocks))
                                    .await
                                    .unwrap();

                                self.sync = sync_out;
                                break;
                            }
                            all::BlockVerification::Success {
                                is_new_best: true,
                                sync: sync_out,
                                ..
                            } => {
                                span.record("outcome", &"success");
                                span.record("is_new_best", &true);

                                // Processing has made a step forward.
                                // There is nothing to do, but this is used to update to best block
                                // shown on the informant.
                                let mut lock = self.sync_state.lock().await;
                                lock.best_block_hash = sync_out.best_block_hash();
                                lock.best_block_number = sync_out.best_block_number();
                                drop(lock);

                                self.sync = sync_out;
                                break;
                            }
                            all::BlockVerification::Success { sync: sync_out, .. } => {
                                span.record("outcome", &"success");
                                span.record("is_new_best", &false);
                                self.sync = sync_out;
                                break;
                            }

                            all::BlockVerification::FinalizedStorageGet(req) => {
                                let value = self
                                    .finalized_block_storage
                                    .get(&req.key_as_vec())
                                    .map(|v| &v[..]);
                                verify = req.inject_value(value);
                            }
                            all::BlockVerification::FinalizedStorageNextKey(req) => {
                                // TODO: to_vec() :-/
                                let req_key = req.key().as_ref().to_vec();
                                // TODO: to_vec() :-/
                                let next_key = self
                                    .finalized_block_storage
                                    .range(req.key().as_ref().to_vec()..)
                                    .find(move |(k, _)| k[..] > req_key[..])
                                    .map(|(k, _)| k);
                                verify = req.inject_key(next_key);
                            }
                            all::BlockVerification::FinalizedStoragePrefixKeys(req) => {
                                // TODO: to_vec() :-/
                                let prefix = req.prefix().as_ref().to_vec();
                                // TODO: to_vec() :-/
                                let keys = self
                                    .finalized_block_storage
                                    .range(req.prefix().as_ref().to_vec()..)
                                    .take_while(|(k, _)| k.starts_with(&prefix))
                                    .map(|(k, _)| k);
                                verify = req.inject_keys_ordered(keys);
                            }
                        }
                    }
                }

                all::ProcessOne::VerifyHeader(verify) => {
                    let hash_to_verify = verify.hash();
                    let height_to_verify = verify.height();

                    let span = tracing::trace_span!(
                        "header-verification",
                        hash_to_verify = %HashDisplay(&hash_to_verify), height = %height_to_verify,
                        outcome = tracing::field::Empty, error = tracing::field::Empty,
                    );
                    let _enter = span.enter();

                    match verify.perform(unix_time, ()) {
                        all::HeaderVerifyOutcome::Success { sync: sync_out, .. } => {
                            span.record("outcome", &"success");
                            self.sync = sync_out;
                            continue;
                        }
                        all::HeaderVerifyOutcome::Error {
                            sync: sync_out,
                            error,
                            ..
                        } => {
                            span.record("outcome", &"failure");
                            span.record("error", &tracing::field::display(error));
                            self.sync = sync_out;
                            continue;
                        }
                    }
                }
            }
        }

        self
    }

    #[tracing::instrument(skip(self))]
    async fn run(mut self) {
        loop {
            self.start_requests().await;
            self = self.process_blocks().await;

            // Update the current best block, used for CLI-related purposes.
            {
                let mut lock = self.sync_state.lock().await;
                lock.best_block_hash = self.sync.best_block_hash();
                lock.best_block_number = self.sync.best_block_number();
            }

            futures::select! {
                network_event = self.from_network_service.next() => {
                    let network_event = match network_event {
                        Some(m) => m,
                        None => return,
                    };

                    match network_event {
                        network_service::Event::Connected { peer_id, chain_index, best_block_number, best_block_hash }
                            if chain_index == self.network_chain_index =>
                        {
                            let id = self.sync.add_source(peer_id.clone(), best_block_number, best_block_hash);
                            self.peers_source_id_map.insert(peer_id, id);
                        },
                        network_service::Event::Disconnected { peer_id, chain_index }
                            if chain_index == self.network_chain_index =>
                        {
                            let id = self.peers_source_id_map.remove(&peer_id).unwrap();
                            let (_, requests) = self.sync.remove_source(id);
                            for (_, abort) in requests {
                                abort.abort();
                            }
                        },
                        network_service::Event::BlockAnnounce { chain_index, peer_id, announce }
                            if chain_index == self.network_chain_index =>
                        {
                            let id = *self.peers_source_id_map.get(&peer_id).unwrap();
                            let decoded = announce.decode();
                            // TODO: stupid to re-encode header
                            // TODO: log the outcome
                            match self.sync.block_announce(id, decoded.header.scale_encoding_vec(), decoded.is_best) {
                                all::BlockAnnounceOutcome::HeaderVerify => {},
                                all::BlockAnnounceOutcome::TooOld { .. } => {},
                                all::BlockAnnounceOutcome::AlreadyInChain => {},
                                all::BlockAnnounceOutcome::NotFinalizedChain => {},
                                all::BlockAnnounceOutcome::InvalidHeader(_) => {},
                                all::BlockAnnounceOutcome::Discarded => {},
                                all::BlockAnnounceOutcome::Disjoint {} => {},
                            }
                        },
                        _ => {
                            // Different chain index.
                        }
                    }
                },

                (request_id, result) = self.block_requests_finished.select_next_some() => {
                    // `result` is an error if the block request got cancelled by the sync state
                    // machine.
                    // TODO: clarify this piece of code
                    if let Ok(result) = result {
                        let result = result.map_err(|_| ());
                        let (_, response_outcome) = self.sync.blocks_request_response(request_id, result.map(|v| v.into_iter().map(|block| all::BlockRequestSuccessBlock {
                            scale_encoded_header: block.header.unwrap(), // TODO: don't unwrap
                            scale_encoded_extrinsics: block.body.unwrap(), // TODO: don't unwrap
                            scale_encoded_justification: block.justification,
                            user_data: (),
                        })));

                        match response_outcome {
                            all::ResponseOutcome::Outdated
                            | all::ResponseOutcome::Queued
                            | all::ResponseOutcome::NotFinalizedChain { .. }
                            | all::ResponseOutcome::AllAlreadyInChain { .. } => {
                            }
                            all::ResponseOutcome::WarpSyncFinished { .. } => {
                                unreachable!()
                            }
                        }
                    }
                },
            }
        }
    }
}

/// Starts the task that writes blocks to the database.
#[tracing::instrument(skip(database, messages_rx))]
async fn start_database_write(
    database: Arc<full_sqlite::SqliteFullDatabase>,
    mut messages_rx: mpsc::Receiver<ToDatabase>,
) {
    loop {
        match messages_rx.next().await {
            None => break,
            Some(ToDatabase::FinalizedBlocks(finalized_blocks)) => {
                let span = tracing::trace_span!("blocks-db-write", len = finalized_blocks.len());
                let _enter = span.enter();

                let new_finalized_hash = finalized_blocks.last().map(|lf| lf.header.hash());

                for block in finalized_blocks {
                    // TODO: overhead for building the SCALE encoding of the header
                    let result = database.insert(
                        &block.header.scale_encoding().fold(Vec::new(), |mut a, b| {
                            a.extend_from_slice(b.as_ref());
                            a
                        }),
                        true, // TODO: is_new_best?
                        block.body.iter(),
                        block
                            .storage_top_trie_changes
                            .iter()
                            .map(|(k, v)| (k, v.as_ref())),
                    );

                    match result {
                        Ok(()) => {}
                        Err(full_sqlite::InsertError::Duplicate) => {} // TODO: this should be an error ; right now we silence them because non-finalized blocks aren't loaded from the database at startup, resulting in them being downloaded again
                        Err(err) => panic!("{}", err),
                    }
                }

                if let Some(new_finalized_hash) = new_finalized_hash {
                    database.set_finalized(&new_finalized_hash).unwrap();
                }
            }
        }
    }
}
