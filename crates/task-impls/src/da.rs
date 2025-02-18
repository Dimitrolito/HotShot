// Copyright (c) 2021-2024 Espresso Systems (espressosys.com)
// This file is part of the HotShot repository.

// You should have received a copy of the MIT License
// along with the HotShot repository. If not, see <https://mit-license.org/>.

use std::{marker::PhantomData, sync::Arc};

use async_broadcast::{Receiver, Sender};
use async_lock::RwLock;
use async_trait::async_trait;
use hotshot_task::task::TaskState;
use hotshot_types::{
    consensus::{Consensus, OuterConsensus},
    data::{DaProposal, PackedBundle},
    event::{Event, EventType},
    message::{Proposal, UpgradeLock},
    simple_certificate::DaCertificate,
    simple_vote::{DaData, DaVote},
    traits::{
        block_contents::vid_commitment,
        election::Membership,
        network::ConnectedNetwork,
        node_implementation::{NodeImplementation, NodeType, Versions},
        signature_key::SignatureKey,
        storage::Storage,
    },
    vote::HasViewNumber,
};
use sha2::{Digest, Sha256};
use tokio::{spawn, task::spawn_blocking};
use tracing::instrument;
use utils::anytrace::*;

use crate::{
    events::HotShotEvent,
    helpers::broadcast_event,
    vote_collection::{handle_vote, VoteCollectorsMap},
};

/// Tracks state of a DA task
pub struct DaTaskState<TYPES: NodeType, I: NodeImplementation<TYPES>, V: Versions> {
    /// Output events to application
    pub output_event_stream: async_broadcast::Sender<Event<TYPES>>,

    /// View number this view is executing in.
    pub cur_view: TYPES::View,

    /// Epoch number this node is executing in.
    pub cur_epoch: TYPES::Epoch,

    /// Reference to consensus. Leader will require a read lock on this.
    pub consensus: OuterConsensus<TYPES>,

    /// Membership for the DA committee and quorum committee.
    /// We need the latter only for calculating the proper VID scheme
    /// from the number of nodes in the quorum.
    pub membership: Arc<TYPES::Membership>,

    /// The underlying network
    pub network: Arc<I::Network>,

    /// A map of `DaVote` collector tasks.
    pub vote_collectors: VoteCollectorsMap<TYPES, DaVote<TYPES>, DaCertificate<TYPES>, V>,

    /// This Nodes public key
    pub public_key: TYPES::SignatureKey,

    /// This Nodes private key
    pub private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,

    /// This state's ID
    pub id: u64,

    /// This node's storage ref
    pub storage: Arc<RwLock<I::Storage>>,

    /// Lock for a decided upgrade
    pub upgrade_lock: UpgradeLock<TYPES, V>,
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>, V: Versions> DaTaskState<TYPES, I, V> {
    /// main task event handler
    #[instrument(skip_all, fields(id = self.id, view = *self.cur_view, epoch = *self.cur_epoch), name = "DA Main Task", level = "error", target = "DaTaskState")]
    pub async fn handle(
        &mut self,
        event: Arc<HotShotEvent<TYPES>>,
        event_stream: Sender<Arc<HotShotEvent<TYPES>>>,
    ) -> Result<()> {
        match event.as_ref() {
            HotShotEvent::DaProposalRecv(proposal, sender) => {
                let sender = sender.clone();
                tracing::debug!(
                    "DA proposal received for view: {:?}",
                    proposal.data.view_number()
                );
                // ED NOTE: Assuming that the next view leader is the one who sends DA proposal for this view
                let view = proposal.data.view_number();

                // Allow a DA proposal that is one view older, in case we have voted on a quorum
                // proposal and updated the view.
                //
                // Anything older is discarded because it is no longer relevant.
                ensure!(
                    self.cur_view <= view + 1,
                    "Throwing away DA proposal that is more than one view older"
                );

                ensure!(
                    !self
                      .consensus
                      .read()
                      .await
                      .saved_payloads()
                      .contains_key(&view),
                    info!(
                      "Received DA proposal for view {:?} but we already have a payload for that view.  Throwing it away",
                      view
                    )
                );

                let encoded_transactions_hash = Sha256::digest(&proposal.data.encoded_transactions);
                let view_leader_key = self.membership.leader(view, self.cur_epoch)?;
                ensure!(
                    view_leader_key == sender,
                    warn!(
                      "DA proposal doesn't have expected leader key for view {} \n DA proposal is: {:?}",
                      *view,
                      proposal.data.clone()
                    )
                );

                ensure!(
                    view_leader_key.validate(&proposal.signature, &encoded_transactions_hash),
                    warn!("Could not verify proposal.")
                );

                broadcast_event(
                    Arc::new(HotShotEvent::DaProposalValidated(proposal.clone(), sender)),
                    &event_stream,
                )
                .await;
            }
            HotShotEvent::DaProposalValidated(proposal, sender) => {
                let cur_view = self.consensus.read().await.cur_view();
                ensure!(
                  cur_view <= proposal.data.view_number() + 1,
                  debug!(
                    "Validated DA proposal for prior view but it's too old now Current view {:?}, DA Proposal view {:?}", 
                    cur_view,
                    proposal.data.view_number()
                  )
                );

                // Proposal is fresh and valid, notify the application layer
                broadcast_event(
                    Event {
                        view_number: self.cur_view,
                        event: EventType::DaProposal {
                            proposal: proposal.clone(),
                            sender: sender.clone(),
                        },
                    },
                    &self.output_event_stream,
                )
                .await;

                ensure!(
                    self.membership
                        .has_da_stake(&self.public_key, self.cur_epoch),
                    debug!(
                        "We were not chosen for consensus committee on {:?}",
                        self.cur_view
                    )
                );

                let txns = Arc::clone(&proposal.data.encoded_transactions);
                let num_nodes = self.membership.total_nodes(self.cur_epoch);
                let payload_commitment =
                    spawn_blocking(move || vid_commitment(&txns, num_nodes)).await;
                let payload_commitment = payload_commitment.unwrap();
                self.storage
                    .write()
                    .await
                    .append_da(proposal, payload_commitment)
                    .await
                    .wrap()
                    .context(error!("Failed to append DA proposal to storage"))?;
                let view_number = proposal.data.view_number();
                // Generate and send vote
                let vote = DaVote::create_signed_vote(
                    DaData {
                        payload_commit: payload_commitment,
                    },
                    view_number,
                    &self.public_key,
                    &self.private_key,
                    &self.upgrade_lock,
                )
                .await?;

                tracing::debug!("Sending vote to the DA leader {:?}", vote.view_number());

                broadcast_event(Arc::new(HotShotEvent::DaVoteSend(vote)), &event_stream).await;
                let mut consensus_writer = self.consensus.write().await;

                // Ensure this view is in the view map for garbage collection.

                if let Err(e) = consensus_writer.update_da_view(view_number, payload_commitment) {
                    tracing::trace!("{e:?}");
                }

                // Record the payload we have promised to make available.
                if let Err(e) = consensus_writer.update_saved_payloads(
                    view_number,
                    Arc::clone(&proposal.data.encoded_transactions),
                ) {
                    tracing::trace!("{e:?}");
                }
                // Optimistically calculate and update VID if we know that the primary network is down.
                if self.network.is_primary_down() {
                    let consensus =
                        OuterConsensus::new(Arc::clone(&self.consensus.inner_consensus));
                    let membership = Arc::clone(&self.membership);
                    let pk = self.private_key.clone();
                    let public_key = self.public_key.clone();
                    let chan = event_stream.clone();
                    let current_epoch = self.cur_epoch;
                    spawn(async move {
                        Consensus::calculate_and_update_vid(
                            OuterConsensus::new(Arc::clone(&consensus.inner_consensus)),
                            view_number,
                            membership,
                            &pk,
                            current_epoch,
                        )
                        .await;
                        if let Some(Some(vid_share)) = consensus
                            .read()
                            .await
                            .vid_shares()
                            .get(&view_number)
                            .map(|shares| shares.get(&public_key).cloned())
                        {
                            broadcast_event(
                                Arc::new(HotShotEvent::VidShareRecv(
                                    public_key.clone(),
                                    vid_share.clone(),
                                )),
                                &chan,
                            )
                            .await;
                        }
                    });
                }
            }
            HotShotEvent::DaVoteRecv(ref vote) => {
                tracing::debug!("DA vote recv, Main Task {:?}", vote.view_number());
                // Check if we are the leader and the vote is from the sender.
                let view = vote.view_number();

                ensure!(
                    self.membership.leader(view, self.cur_epoch)? == self.public_key,
                    debug!(
                      "We are not the DA committee leader for view {} are we leader for next view? {}",
                      *view,
                      self.membership.leader(view + 1, self.cur_epoch)? == self.public_key
                    )
                );

                handle_vote(
                    &mut self.vote_collectors,
                    vote,
                    self.public_key.clone(),
                    &self.membership,
                    self.cur_epoch,
                    self.id,
                    &event,
                    &event_stream,
                    &self.upgrade_lock,
                    true,
                )
                .await?;
            }
            HotShotEvent::ViewChange(view, epoch) => {
                if *epoch > self.cur_epoch {
                    self.cur_epoch = *epoch;
                }

                let view = *view;
                ensure!(
                    *self.cur_view < *view,
                    info!("Received a view change to an older view.")
                );

                if *view - *self.cur_view > 1 {
                    tracing::info!("View changed by more than 1 going to view {:?}", view);
                }
                self.cur_view = view;
            }
            HotShotEvent::BlockRecv(packed_bundle) => {
                let PackedBundle::<TYPES> {
                    encoded_transactions,
                    metadata,
                    view_number,
                    ..
                } = packed_bundle;
                let view_number = *view_number;

                // quick hash the encoded txns with sha256
                let encoded_transactions_hash = Sha256::digest(encoded_transactions);

                // sign the encoded transactions as opposed to the VID commitment
                let signature =
                    TYPES::SignatureKey::sign(&self.private_key, &encoded_transactions_hash)
                        .wrap()?;

                let data: DaProposal<TYPES> = DaProposal {
                    encoded_transactions: Arc::clone(encoded_transactions),
                    metadata: metadata.clone(),
                    // Upon entering a new view we want to send a DA Proposal for the next view -> Is it always the case that this is cur_view + 1?
                    view_number,
                };

                let message = Proposal {
                    data,
                    signature,
                    _pd: PhantomData,
                };

                broadcast_event(
                    Arc::new(HotShotEvent::DaProposalSend(
                        message.clone(),
                        self.public_key.clone(),
                    )),
                    &event_stream,
                )
                .await;
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
/// task state implementation for DA Task
impl<TYPES: NodeType, I: NodeImplementation<TYPES>, V: Versions> TaskState
    for DaTaskState<TYPES, I, V>
{
    type Event = HotShotEvent<TYPES>;

    async fn handle_event(
        &mut self,
        event: Arc<Self::Event>,
        sender: &Sender<Arc<Self::Event>>,
        _receiver: &Receiver<Arc<Self::Event>>,
    ) -> Result<()> {
        self.handle(event, sender.clone()).await
    }

    fn cancel_subtasks(&mut self) {}
}
