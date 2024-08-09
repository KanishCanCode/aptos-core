// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    consensus_observer::{
        logging::{LogEntry, LogSchema},
        metrics,
        network_client::ConsensusObserverClient,
        network_handler::ConsensusObserverNetworkMessage,
        network_message::{
            BlockPayload, CommitDecision, ConsensusObserverDirectSend, ConsensusObserverMessage,
            OrderedBlock,
        },
        ordered_blocks::OrderedBlockStore,
        payload_store::BlockPayloadStore,
        pending_blocks::PendingBlockStore,
        publisher::ConsensusPublisher,
        subscription_manager::SubscriptionManager,
    },
    dag::DagCommitSigner,
    network::{IncomingCommitRequest, IncomingRandGenRequest},
    network_interface::CommitMessage,
    payload_manager::{
        ConsensusObserverPayloadManager, DirectMempoolPayloadManager, TPayloadManager,
    },
    pipeline::execution_client::TExecutionClient,
    state_replication::StateComputerCommitCallBackType,
};
use aptos_channels::{aptos_channel, aptos_channel::Receiver, message_queues::QueueStyle};
use aptos_config::config::NodeConfig;
use aptos_consensus_types::{pipeline, pipelined_block::PipelinedBlock};
use aptos_crypto::{bls12381, Genesis};
use aptos_event_notifications::{DbBackedOnChainConfig, ReconfigNotificationListener};
use aptos_infallible::Mutex;
use aptos_logger::{debug, error, info, warn};
use aptos_network::{
    application::interface::NetworkClient, protocols::wire::handshake::v1::ProtocolId,
};
use aptos_reliable_broadcast::DropGuard;
use aptos_storage_interface::DbReader;
use aptos_time_service::TimeService;
use aptos_types::{
    block_info::{BlockInfo, Round},
    epoch_state::EpochState,
    ledger_info::LedgerInfoWithSignatures,
    on_chain_config::{
        OnChainConsensusConfig, OnChainExecutionConfig, OnChainRandomnessConfig,
        RandomnessConfigMoveStruct, RandomnessConfigSeqNum, ValidatorSet,
    },
    validator_signer::ValidatorSigner,
};
use futures::{
    future::{AbortHandle, Abortable},
    StreamExt,
};
use futures_channel::oneshot;
use move_core_types::account_address::AccountAddress;
use std::{sync::Arc, time::Duration};
use tokio::{sync::mpsc::UnboundedSender, time::interval};
use tokio_stream::wrappers::IntervalStream;

// Whether to log messages at the info level (useful for debugging)
const LOG_MESSAGES_AT_INFO_LEVEL: bool = true;

/// The consensus observer receives consensus updates and propagates them to the execution pipeline
pub struct ConsensusObserver {
    // The configuration of the node
    node_config: NodeConfig,

    // The current epoch state
    epoch_state: Option<Arc<EpochState>>,
    // Whether quorum store is enabled (updated on epoch changes)
    quorum_store_enabled: bool,
    // The latest ledger info (updated via a callback)
    root: Arc<Mutex<LedgerInfoWithSignatures>>,

    // The block payload store holds block transaction payloads
    block_payload_store: BlockPayloadStore,
    // The ordered block store holds ordered blocks that are ready for execution
    ordered_block_store: OrderedBlockStore,
    // The pending block store holds pending blocks that are without payloads
    pending_block_store: PendingBlockStore,
    // The execution client to the buffer manager
    execution_client: Arc<dyn TExecutionClient>,

    // If the sync handle is set it indicates that we're in state sync mode.
    // The flag indicates if we're waiting to transition to a new epoch.
    sync_handle: Option<(DropGuard, bool)>,
    // The sender to notify the consensus observer that state sync to the (epoch, round) is done
    sync_notification_sender: UnboundedSender<(u64, Round)>,
    // The reconfiguration event listener to refresh on-chain configs
    reconfig_events: Option<ReconfigNotificationListener<DbBackedOnChainConfig>>,

    // The subscription manager
    subscription_manager: SubscriptionManager,

    // The consensus publisher
    consensus_publisher: Option<Arc<ConsensusPublisher>>,
}

impl ConsensusObserver {
    pub fn new(
        node_config: NodeConfig,
        consensus_observer_client: Arc<
            ConsensusObserverClient<NetworkClient<ConsensusObserverMessage>>,
        >,
        db_reader: Arc<dyn DbReader>,
        execution_client: Arc<dyn TExecutionClient>,
        sync_notification_sender: UnboundedSender<(u64, Round)>,
        reconfig_events: Option<ReconfigNotificationListener<DbBackedOnChainConfig>>,
        consensus_publisher: Option<Arc<ConsensusPublisher>>,
        time_service: TimeService,
    ) -> Self {
        // Read the latest ledger info from storage
        let root = db_reader
            .get_latest_ledger_info()
            .expect("Failed to read latest ledger info!");

        // Get the consensus observer config
        let consensus_observer_config = node_config.consensus_observer;

        // Create the subscription manager
        let subscription_manager = SubscriptionManager::new(
            consensus_observer_client,
            node_config.consensus_observer,
            consensus_publisher.clone(),
            db_reader.clone(),
            time_service.clone(),
        );

        // Create the consensus observer
        Self {
            node_config,
            epoch_state: None,
            quorum_store_enabled: false, // Updated on epoch changes
            root: Arc::new(Mutex::new(root)),
            ordered_block_store: OrderedBlockStore::new(consensus_observer_config),
            block_payload_store: BlockPayloadStore::new(consensus_observer_config),
            pending_block_store: PendingBlockStore::new(consensus_observer_config),
            execution_client,
            sync_handle: None,
            sync_notification_sender,
            reconfig_events,
            subscription_manager,
            consensus_publisher,
        }
    }

    /// Returns true iff all payloads exist for the given blocks
    fn all_payloads_exist(&self, blocks: &[Arc<PipelinedBlock>]) -> bool {
        // If quorum store is disabled, all payloads exist (they're already in the blocks)
        if !self.quorum_store_enabled {
            return true;
        }

        // Otherwise, check if all the payloads exist in the payload store
        self.block_payload_store.all_payloads_exist(blocks)
    }

    /// Checks the progress of the consensus observer
    async fn check_progress(&mut self) {
        debug!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("Checking consensus observer progress!"));

        // If we're in state sync mode, we should wait for state sync to complete
        if self.in_state_sync_mode() {
            info!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Waiting for state sync to reach target: {:?}!",
                    self.root.lock().commit_info()
                ))
            );
            return;
        }

        // Otherwise, check the health of the active subscription
        let new_subscription_created = self
            .subscription_manager
            .check_and_manage_subscriptions()
            .await;
        if new_subscription_created {
            // Clear the pending block state (a new subscription was created)
            self.clear_pending_block_state().await;
        }
    }

    /// Clears the pending block state (this is useful for changing
    /// subscriptions, where we want to wipe all state and restart).
    async fn clear_pending_block_state(&self) {
        // Clear the payload store
        self.block_payload_store.clear_all_payloads();

        // Clear the pending blocks
        self.pending_block_store.clear_missing_blocks();

        // Clear the ordered blocks
        self.ordered_block_store.clear_all_ordered_blocks();

        // Reset the execution pipeline for the root
        let root = self.root.lock().clone();
        if let Err(error) = self.execution_client.reset(&root).await {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to reset the execution pipeline for the root! Error: {:?}",
                    error
                ))
            );
        }
    }

    /// Creates and returns a commit callback (to be called after the execution pipeline)
    fn create_commit_callback(&self) -> StateComputerCommitCallBackType {
        // Clone the root, pending blocks and payload store
        let root = self.root.clone();
        let pending_ordered_blocks = self.ordered_block_store.clone();
        let block_payload_store = self.block_payload_store.clone();

        // Create the commit callback
        Box::new(move |blocks, ledger_info: LedgerInfoWithSignatures| {
            // Remove the committed blocks from the payload and pending stores
            block_payload_store.remove_committed_blocks(blocks);
            pending_ordered_blocks.remove_blocks_for_commit(&ledger_info);

            // Verify the ledger info is for the same epoch
            let mut root = root.lock();
            if ledger_info.commit_info().epoch() != root.commit_info().epoch() {
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Received commit callback for a different epoch! Ledger info: {:?}, Root: {:?}",
                        ledger_info.commit_info(),
                        root.commit_info()
                    ))
                );
                return;
            }

            // Update the root ledger info. Note: we only want to do this if
            // the new ledger info round is greater than the current root
            // round. Otherwise, this can race with the state sync process.
            if ledger_info.commit_info().round() > root.commit_info().round() {
                *root = ledger_info;
            }
        })
    }

    /// Finalizes the ordered block by sending it to the execution pipeline
    async fn finalize_ordered_block(&mut self, ordered_block: OrderedBlock) {
        info!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Forwarding ordered blocks to the execution pipeline: {}",
                ordered_block.proof_block_info()
            ))
        );

        // Send the ordered block to the execution pipeline
        if let Err(error) = self
            .execution_client
            .finalize_order(
                ordered_block.blocks(),
                ordered_block.ordered_proof().clone(),
                self.create_commit_callback(),
            )
            .await
        {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to finalize ordered block! Error: {:?}",
                    error
                ))
            );
        }
    }

    /// Forwards the commit decision to the execution pipeline
    fn forward_commit_decision(&self, commit_decision: CommitDecision) {
        // Create a dummy RPC message
        let (response_sender, _response_receiver) = oneshot::channel();
        let commit_request = IncomingCommitRequest {
            req: CommitMessage::Decision(pipeline::commit_decision::CommitDecision::new(
                commit_decision.commit_proof().clone(),
            )),
            protocol: ProtocolId::ConsensusDirectSendCompressed,
            response_sender,
        };

        // Send the message to the execution client
        if let Err(error) = self
            .execution_client
            .send_commit_msg(AccountAddress::ONE, commit_request)
        {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to send commit decision to the execution pipeline! Error: {:?}",
                    error
                ))
            )
        };
    }

    /// Returns the current epoch state, and panics if it is not set
    fn get_epoch_state(&self) -> Arc<EpochState> {
        self.epoch_state
            .clone()
            .expect("The epoch state is not set! This should never happen!")
    }

    /// Returns the last known block
    fn get_last_block(&self) -> BlockInfo {
        if let Some(last_pending_block) = self.ordered_block_store.get_last_ordered_block() {
            last_pending_block
        } else {
            // Return the root ledger info
            self.root.lock().commit_info().clone()
        }
    }

    /// Returns true iff we are waiting for state sync to complete an epoch change
    fn in_state_sync_epoch_change(&self) -> bool {
        matches!(self.sync_handle, Some((_, true)))
    }

    /// Returns true iff we are waiting for state sync to complete
    fn in_state_sync_mode(&self) -> bool {
        self.sync_handle.is_some()
    }

    /// Orders any ready pending blocks for the given epoch and round
    async fn order_ready_pending_block(&mut self, block_epoch: u64, block_round: Round) {
        if let Some(ordered_block) = self.pending_block_store.remove_ready_block(
            block_epoch,
            block_round,
            &self.block_payload_store,
        ) {
            self.process_ordered_block(ordered_block).await;
        }
    }

    /// Processes the block payload message
    async fn process_block_payload_message(&mut self, block_payload: BlockPayload) {
        // Get the epoch and round for the block
        let block_epoch = block_payload.block.epoch();
        let block_round = block_payload.block.round();

        // Update the metrics for the received block payload
        metrics::set_gauge_with_label(
            &metrics::OBSERVER_RECEIVED_MESSAGE_ROUNDS,
            metrics::BLOCK_PAYLOAD_LABEL,
            block_round,
        );

        // Verify the block payload digests
        if let Err(error) = block_payload.verify_payload_digests() {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to verify block payload digests! Ignoring block: {:?}. Error: {:?}",
                    block_payload.block, error
                ))
            );
            return;
        }

        // If the payload is for the current epoch, verify the proof signatures
        let epoch_state = self.get_epoch_state();
        let verified_payload = if block_epoch == epoch_state.epoch {
            // Verify the block proof signatures
            if let Err(error) = block_payload.verify_payload_signatures(&epoch_state) {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to verify block payload signatures! Ignoring block: {:?}. Error: {:?}",
                        block_payload.block, error
                    ))
                );
                return;
            }

            true // We have successfully verified the signatures
        } else {
            false // We can't verify the signatures yet
        };

        // Update the payload store with the payload
        self.block_payload_store
            .insert_block_payload(block_payload, verified_payload);

        // Check if there are blocks that were missing payloads but are
        // now ready because of the new payload. Note: this should only
        // be done if the payload has been verified correctly.
        if verified_payload {
            self.order_ready_pending_block(block_epoch, block_round)
                .await;
        }
    }

    /// Processes the commit decision message
    fn process_commit_decision_message(&mut self, commit_decision: CommitDecision) {
        // Update the metrics for the received commit decision
        metrics::set_gauge_with_label(
            &metrics::OBSERVER_RECEIVED_MESSAGE_ROUNDS,
            metrics::COMMIT_DECISION_LABEL,
            commit_decision.round(),
        );

        // If the commit decision is for the current epoch, verify and process it
        let epoch_state = self.get_epoch_state();
        let commit_decision_epoch = commit_decision.epoch();
        if commit_decision_epoch == epoch_state.epoch {
            // Verify the commit decision
            if let Err(error) = commit_decision.verify_commit_proof(&epoch_state) {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to verify commit decision! Ignoring: {:?}, Error: {:?}",
                        commit_decision.proof_block_info(),
                        error
                    ))
                );
                return;
            }

            // Update the pending blocks with the commit decision
            if self.process_commit_decision_for_pending_block(&commit_decision) {
                return; // The commit decision was successfully processed
            }
        }

        // TODO: identify the best way to handle an invalid commit decision
        // for a future epoch. In such cases, we currently rely on state sync.

        // Otherwise, we failed to process the commit decision. If the commit
        // is for a future epoch or round, we need to state sync.
        let last_block = self.get_last_block();
        let commit_decision_round = commit_decision.round();
        let epoch_changed = commit_decision_epoch > last_block.epoch();
        if epoch_changed || commit_decision_round > last_block.round() {
            // If we're waiting for state sync to transition into a new epoch,
            // we should just wait and not issue a new state sync request.
            if self.in_state_sync_epoch_change() {
                info!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Already waiting for state sync to reach new epoch: {:?}. Dropping commit decision: {:?}!",
                        self.root.lock().commit_info(),
                        commit_decision.proof_block_info()
                    ))
                );
                return;
            }

            // Otherwise, we should start the state sync process
            info!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Started syncing to {}!",
                    commit_decision.proof_block_info()
                ))
            );

            // Update the root and clear the pending blocks (up to the commit)
            *self.root.lock() = commit_decision.commit_proof().clone();
            self.block_payload_store
                .remove_blocks_for_epoch_round(commit_decision_epoch, commit_decision_round);
            self.ordered_block_store
                .remove_blocks_for_commit(commit_decision.commit_proof());

            // Start the state sync process
            let abort_handle = sync_to_commit_decision(
                commit_decision,
                commit_decision_epoch,
                commit_decision_round,
                self.execution_client.clone(),
                self.sync_notification_sender.clone(),
            );
            self.sync_handle = Some((DropGuard::new(abort_handle), epoch_changed));
        }
    }

    /// Processes the commit decision for the pending block and returns true iff
    /// the commit decision was successfully processed. Note: this function
    /// assumes the commit decision has already been verified.
    fn process_commit_decision_for_pending_block(&self, commit_decision: &CommitDecision) -> bool {
        // Get the pending block for the commit decision
        let pending_block = self
            .ordered_block_store
            .get_ordered_block(commit_decision.epoch(), commit_decision.round());

        // Process the pending block
        if let Some(pending_block) = pending_block {
            // If all payloads exist, add the commit decision to the pending blocks
            if self.all_payloads_exist(pending_block.blocks()) {
                debug!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Adding decision to pending block: {}",
                        commit_decision.proof_block_info()
                    ))
                );
                self.ordered_block_store
                    .update_commit_decision(commit_decision);

                // If we are not in sync mode, forward the commit decision to the execution pipeline
                if !self.in_state_sync_mode() {
                    info!(
                        LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                            "Forwarding commit decision to the execution pipeline: {}",
                            commit_decision.proof_block_info()
                        ))
                    );
                    self.forward_commit_decision(commit_decision.clone());
                }

                return true; // The commit decision was successfully processed
            }
        }

        false // The commit decision was not processed
    }

    /// Processes a network message received by the consensus observer
    async fn process_network_message(&mut self, network_message: ConsensusObserverNetworkMessage) {
        // Unpack the network message
        let (peer_network_id, message) = network_message.into_parts();

        // Verify the message is from the peer we've subscribed to
        if let Err(error) = self
            .subscription_manager
            .verify_message_sender(peer_network_id)
        {
            warn!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Message failed subscription sender verification! Error: {:?}",
                    error,
                ))
            );

            return;
        }

        // Increment the received message counter
        metrics::increment_request_counter(
            &metrics::OBSERVER_RECEIVED_MESSAGES,
            message.get_label(),
            &peer_network_id,
        );

        // Process the message based on the type
        match message {
            ConsensusObserverDirectSend::OrderedBlock(ordered_block) => {
                // Log the received ordered block message
                let log_message = format!(
                    "Received ordered block: {}, from peer: {}!",
                    ordered_block.proof_block_info(),
                    peer_network_id
                );
                log_received_message(log_message);

                // Process the ordered block message
                self.process_ordered_block_message(ordered_block).await;
            },
            ConsensusObserverDirectSend::CommitDecision(commit_decision) => {
                // Log the received commit decision message
                let log_message = format!(
                    "Received commit decision: {}, from peer: {}!",
                    commit_decision.proof_block_info(),
                    peer_network_id
                );
                log_received_message(log_message);

                // Process the commit decision message
                self.process_commit_decision_message(commit_decision);
            },
            ConsensusObserverDirectSend::BlockPayload(block_payload) => {
                // Log the received block payload message
                let log_message = format!(
                    "Received block payload: {}, from peer: {}!",
                    block_payload.block, peer_network_id
                );
                log_received_message(log_message);

                // Process the block payload message
                self.process_block_payload_message(block_payload).await;
            },
        }

        // Update the metrics for the processed blocks
        self.update_processed_blocks_metrics();
    }

    /// Processes the ordered block
    async fn process_ordered_block_message(&mut self, ordered_block: OrderedBlock) {
        // Verify the ordered blocks before processing
        if let Err(error) = ordered_block.verify_ordered_blocks() {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to verify ordered blocks! Ignoring: {:?}, Error: {:?}",
                    ordered_block.proof_block_info(),
                    error
                ))
            );
            return;
        };

        // If all payloads exist, process the block. Otherwise, store it
        // in the pending block store and wait for the payloads to arrive.
        if self.all_payloads_exist(ordered_block.blocks()) {
            self.process_ordered_block(ordered_block).await;
        } else {
            self.pending_block_store.insert_pending_block(ordered_block);
        }
    }

    /// Processes the ordered block. This assumes the ordered block
    /// has been sanity checked and that all payloads exist.
    async fn process_ordered_block(&mut self, ordered_block: OrderedBlock) {
        // Verify the ordered block proof
        let epoch_state = self.get_epoch_state();
        if ordered_block.proof_block_info().epoch() == epoch_state.epoch {
            if let Err(error) = ordered_block.verify_ordered_proof(&epoch_state) {
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to verify ordered proof! Ignoring: {:?}, Error: {:?}",
                        ordered_block.proof_block_info(),
                        error
                    ))
                );
                return;
            }
        } else {
            // Drop the block and log an error (the block should always be for the current epoch)
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Received ordered block for a different epoch! Ignoring: {:?}",
                    ordered_block.proof_block_info()
                ))
            );
            return;
        };

        // Verify the block payloads against the ordered block
        if let Err(error) = self
            .block_payload_store
            .verify_payloads_against_ordered_block(&ordered_block)
        {
            error!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Failed to verify block payloads against ordered block! Ignoring: {:?}, Error: {:?}",
                    ordered_block.proof_block_info(),
                    error
                ))
            );
            return;
        }

        // The block was verified correctly. If the block is a child of our
        // last block, we can insert it into the ordered block store.
        if self.get_last_block().id() == ordered_block.first_block().parent_id() {
            // Insert the ordered block into the pending blocks
            self.ordered_block_store
                .insert_ordered_block(ordered_block.clone());

            // If we're not in sync mode, finalize the ordered blocks
            if !self.in_state_sync_mode() {
                self.finalize_ordered_block(ordered_block).await;
            }
        } else {
            warn!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                    "Parent block for ordered block is missing! Ignoring: {:?}",
                    ordered_block.proof_block_info()
                ))
            );
        }
    }

    /// Processes the sync complete notification for the given epoch and round
    async fn process_sync_notification(&mut self, epoch: u64, round: Round) {
        // Log the sync notification
        info!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Received sync complete notification for epoch {}, round: {}",
                epoch, round
            ))
        );

        // Verify that the sync notification is for the current epoch and round
        if !check_root_epoch_and_round(self.root.clone(), epoch, round) {
            info!(
                LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Received invalid sync notification for epoch: {}, round: {}! Current root: {:?}",
                epoch, round, self.root
                ))
            );
            return;
        }

        // If the epoch has changed, end the current epoch and start the new one
        let current_epoch_state = self.get_epoch_state();
        if epoch > current_epoch_state.epoch {
            // Wait for the next epoch to start
            self.execution_client.end_epoch().await;
            self.wait_for_epoch_start().await;

            // Verify the block payloads for the new epoch
            let new_epoch_state = self.get_epoch_state();
            let verified_payload_rounds = self
                .block_payload_store
                .verify_payload_signatures(&new_epoch_state);

            // Order all the pending blocks that are now ready (these were buffered during state sync)
            for payload_round in verified_payload_rounds {
                self.order_ready_pending_block(new_epoch_state.epoch, payload_round)
                    .await;
            }
        };

        // Reset and drop the sync handle
        self.sync_handle = None;

        // Process all the newly ordered blocks
        for (_, (ordered_block, commit_decision)) in
            self.ordered_block_store.get_all_ordered_blocks()
        {
            // Finalize the ordered block
            self.finalize_ordered_block(ordered_block).await;

            // If a commit decision is available, forward it to the execution pipeline
            if let Some(commit_decision) = commit_decision {
                self.forward_commit_decision(commit_decision.clone());
            }
        }
    }

    /// Updates the metrics for the processed blocks
    fn update_processed_blocks_metrics(&self) {
        // Update the payload store metrics
        self.block_payload_store.update_payload_store_metrics();

        // Update the pending block metrics
        self.pending_block_store.update_pending_blocks_metrics();

        // Update the pending block metrics
        self.ordered_block_store.update_ordered_blocks_metrics();
    }

    /// Waits for a new epoch to start
    async fn wait_for_epoch_start(&mut self) {
        // Extract the epoch state and on-chain configs
        let (epoch_state, consensus_config, execution_config, randomness_config) = if let Some(
            reconfig_events,
        ) =
            &mut self.reconfig_events
        {
            extract_on_chain_configs(&self.node_config, reconfig_events).await
        } else {
            panic!("Reconfig events are required to wait for a new epoch to start! Something has gone wrong!")
        };

        // Update the local epoch state and quorum store config
        self.epoch_state = Some(epoch_state.clone());
        self.quorum_store_enabled = consensus_config.quorum_store_enabled();
        info!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "New epoch started: {:?}. Updated the epoch state! Quorum store enabled: {:?}",
                epoch_state.epoch, self.quorum_store_enabled,
            ))
        );

        // Create the payload manager
        let payload_manager: Arc<dyn TPayloadManager> = if self.quorum_store_enabled {
            Arc::new(ConsensusObserverPayloadManager::new(
                self.block_payload_store.get_block_payloads(),
                self.consensus_publisher.clone(),
            ))
        } else {
            Arc::new(DirectMempoolPayloadManager {})
        };

        // Start the new epoch
        let signer = Arc::new(ValidatorSigner::new(
            AccountAddress::ZERO,
            bls12381::PrivateKey::genesis(),
        ));
        let dummy_signer = Arc::new(DagCommitSigner::new(signer.clone()));
        let (_, rand_msg_rx) =
            aptos_channel::new::<AccountAddress, IncomingRandGenRequest>(QueueStyle::FIFO, 1, None);
        self.execution_client
            .start_epoch(
                epoch_state.clone(),
                dummy_signer,
                payload_manager,
                &consensus_config,
                &execution_config,
                &randomness_config,
                None,
                None,
                rand_msg_rx,
                0,
            )
            .await;
    }

    /// Starts the consensus observer loop that processes incoming
    /// messages and ensures the observer is making progress.
    pub async fn start(
        mut self,
        mut consensus_observer_message_receiver: Receiver<(), ConsensusObserverNetworkMessage>,
        mut sync_notification_listener: tokio::sync::mpsc::UnboundedReceiver<(u64, Round)>,
    ) {
        // Create a progress check ticker
        let mut progress_check_interval = IntervalStream::new(interval(Duration::from_millis(
            self.node_config
                .consensus_observer
                .progress_check_interval_ms,
        )))
        .fuse();

        // Wait for the epoch to start
        self.wait_for_epoch_start().await;

        // Start the consensus observer loop
        info!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("Starting the consensus observer loop!"));
        loop {
            tokio::select! {
                Some(network_message) = consensus_observer_message_receiver.next() => {
                    self.process_network_message(network_message).await;
                }
                Some((epoch, round)) = sync_notification_listener.recv() => {
                    self.process_sync_notification(epoch, round).await;
                },
                _ = progress_check_interval.select_next_some() => {
                    self.check_progress().await;
                }
                else => {
                    break; // Exit the consensus observer loop
                }
            }
        }

        // Log the exit of the consensus observer loop
        error!(LogSchema::new(LogEntry::ConsensusObserver)
            .message("The consensus observer loop exited unexpectedly!"));
    }
}

/// Checks that the epoch and round match the current root
fn check_root_epoch_and_round(
    root: Arc<Mutex<LedgerInfoWithSignatures>>,
    epoch: u64,
    round: Round,
) -> bool {
    // Get the expected epoch and round
    let root = root.lock();
    let expected_epoch = root.commit_info().epoch();
    let expected_round = root.commit_info().round();

    // Check if the expected epoch and round match
    expected_epoch == epoch && expected_round == round
}

/// A simple helper function that extracts the on-chain configs from the reconfig events
async fn extract_on_chain_configs(
    node_config: &NodeConfig,
    reconfig_events: &mut ReconfigNotificationListener<DbBackedOnChainConfig>,
) -> (
    Arc<EpochState>,
    OnChainConsensusConfig,
    OnChainExecutionConfig,
    OnChainRandomnessConfig,
) {
    // Fetch the next reconfiguration notification
    let reconfig_notification = reconfig_events
        .next()
        .await
        .expect("Failed to get reconfig notification!");

    // Extract the epoch state from the reconfiguration notification
    let on_chain_configs = reconfig_notification.on_chain_configs;
    let validator_set: ValidatorSet = on_chain_configs
        .get()
        .expect("Failed to get the validator set from the on-chain configs!");
    let epoch_state = Arc::new(EpochState {
        epoch: on_chain_configs.epoch(),
        verifier: (&validator_set).into(),
    });

    // Extract the consensus config (or use the default if it's missing)
    let onchain_consensus_config: anyhow::Result<OnChainConsensusConfig> = on_chain_configs.get();
    if let Err(error) = &onchain_consensus_config {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain consensus config! Error: {:?}",
                error
            ))
        );
    }
    let consensus_config = onchain_consensus_config.unwrap_or_default();

    // Extract the execution config (or use the default if it's missing)
    let onchain_execution_config: anyhow::Result<OnChainExecutionConfig> = on_chain_configs.get();
    if let Err(error) = &onchain_execution_config {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain execution config! Error: {:?}",
                error
            ))
        );
    }
    let execution_config =
        onchain_execution_config.unwrap_or_else(|_| OnChainExecutionConfig::default_if_missing());

    // Extract the randomness config sequence number (or use the default if it's missing)
    let onchain_randomness_config_seq_num: anyhow::Result<RandomnessConfigSeqNum> =
        on_chain_configs.get();
    if let Err(error) = &onchain_randomness_config_seq_num {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain randomness config seq num! Error: {:?}",
                error
            ))
        );
    }
    let onchain_randomness_config_seq_num = onchain_randomness_config_seq_num
        .unwrap_or_else(|_| RandomnessConfigSeqNum::default_if_missing());

    // Extract the randomness config
    let onchain_randomness_config: anyhow::Result<RandomnessConfigMoveStruct> =
        on_chain_configs.get();
    if let Err(error) = &onchain_randomness_config {
        error!(
            LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                "Failed to read on-chain randomness config! Error: {:?}",
                error
            ))
        );
    }
    let onchain_randomness_config = OnChainRandomnessConfig::from_configs(
        node_config.randomness_override_seq_num,
        onchain_randomness_config_seq_num.seq_num,
        onchain_randomness_config.ok(),
    );

    // Return the extracted epoch state and on-chain configs
    (
        epoch_state,
        consensus_config,
        execution_config,
        onchain_randomness_config,
    )
}

/// Logs the received message using an appropriate log level
fn log_received_message(message: String) {
    // Log the message at the appropriate level
    let log_schema = LogSchema::new(LogEntry::ConsensusObserver).message(&message);
    if LOG_MESSAGES_AT_INFO_LEVEL {
        info!(log_schema);
    } else {
        debug!(log_schema);
    }
}

/// Spawns a task to sync to the given commit decision and notifies
/// the consensus observer. Also, returns an abort handle to cancel the task.
#[allow(clippy::unwrap_used)]
fn sync_to_commit_decision(
    commit_decision: CommitDecision,
    decision_epoch: u64,
    decision_round: Round,
    execution_client: Arc<dyn TExecutionClient>,
    sync_notification_sender: UnboundedSender<(u64, Round)>,
) -> AbortHandle {
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    tokio::spawn(Abortable::new(
        async move {
            // Sync to the commit decision
            if let Err(error) = execution_client
                .clone()
                .sync_to(commit_decision.commit_proof().clone())
                .await
            {
                warn!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to sync to commit decision: {:?}! Error: {:?}",
                        commit_decision, error
                    ))
                );
            }

            // Notify the consensus observer that the sync is complete
            if let Err(error) = sync_notification_sender.send((decision_epoch, decision_round)) {
                error!(
                    LogSchema::new(LogEntry::ConsensusObserver).message(&format!(
                        "Failed to send sync notification for decision epoch: {:?}, round: {:?}! Error: {:?}",
                        decision_epoch, decision_round, error
                    ))
                );
            }
        },
        abort_registration,
    ));
    abort_handle
}
