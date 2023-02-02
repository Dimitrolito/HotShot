//! Network message types
//!
//! This module contains types used to represent the various types of messages that
//! `HotShot` nodes can send among themselves.

use crate::{
    data::{LeafType, ProposalType},
    traits::{
        node_implementation::NodeType,
        signature_key::{EncodedPublicKey, EncodedSignature},
    },
};
use commit::Commitment;
use derivative::Derivative;
use serde::{Deserialize, Serialize};

/// Incoming message
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct Message<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    PROPOSAL: ProposalType<NodeType = TYPES>,
> {
    /// The sender of this message
    pub sender: TYPES::SignatureKey,

    /// The message kind
    pub kind: MessageKind<TYPES, LEAF, PROPOSAL>,
}

// TODO (da) make it more customized to the consensus layer, maybe separating the specific message
// data from the kind enum.
/// Enum representation of any message type
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub enum MessageKind<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    PROPOSAL: ProposalType<NodeType = TYPES>,
> {
    /// Messages related to the consensus protocol
    Consensus(ConsensusMessage<TYPES, LEAF, PROPOSAL>),
    /// Messages relating to sharing data between nodes
    Data(DataMessage<TYPES, LEAF>),
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
    > From<ConsensusMessage<TYPES, LEAF, PROPOSAL>> for MessageKind<TYPES, LEAF, PROPOSAL>
{
    fn from(m: ConsensusMessage<TYPES, LEAF, PROPOSAL>) -> Self {
        Self::Consensus(m)
    }
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
    > From<DataMessage<TYPES, LEAF>> for MessageKind<TYPES, LEAF, PROPOSAL>
{
    fn from(m: DataMessage<TYPES, LEAF>) -> Self {
        Self::Data(m)
    }
}

// TODO (da) Modify the Vote enum after the consensus trait refactoring:
// <https://github.com/EspressoSystems/HotShot/issues/856>.
/// Votes sent by consensus messages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub enum Vote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// The vote on DA proposal.
    DA(DAVote<TYPES, LEAF>),
    /// Posivite vote on validating or commitment proposal.
    Yes(YesOrNoVote<TYPES, LEAF>),
    /// Negative vote on validating or commitment proposal.
    No(YesOrNoVote<TYPES, LEAF>),
    /// Timeout vote.
    Timeout(TimeoutVote<TYPES, LEAF>),
}

/// Internal triggers sent by consensus messages.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub enum InternalTrigger<TYPES: NodeType> {
    /// Internal timeout at the specified view number.
    Timeout(TYPES::Time), // May add other triggers if necessary.
}

/// a processed consensus message
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub enum ProcessedConsensusMessage<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    PROPOSAL: ProposalType<NodeType = TYPES>,
> {
    /// Leader's proposal
    Proposal(Proposal<PROPOSAL>, TYPES::SignatureKey),
    /// Replica's vote on a proposal.
    Vote(Vote<TYPES, LEAF>, TYPES::SignatureKey),
    /// Internal ONLY message indicating an view interrupt.
    #[serde(skip)]
    InternalTrigger(InternalTrigger<TYPES>),
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
    > From<ProcessedConsensusMessage<TYPES, LEAF, PROPOSAL>>
    for ConsensusMessage<TYPES, LEAF, PROPOSAL>
{
    /// row polymorphism would be great here
    fn from(value: ProcessedConsensusMessage<TYPES, LEAF, PROPOSAL>) -> Self {
        match value {
            ProcessedConsensusMessage::Proposal(p, _) => ConsensusMessage::Proposal(p),
            ProcessedConsensusMessage::Vote(v, _) => ConsensusMessage::Vote(v),
            ProcessedConsensusMessage::InternalTrigger(a) => ConsensusMessage::InternalTrigger(a),
        }
    }
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
    > ProcessedConsensusMessage<TYPES, LEAF, PROPOSAL>
{
    /// row polymorphism would be great here
    pub fn new(
        value: ConsensusMessage<TYPES, LEAF, PROPOSAL>,
        sender: TYPES::SignatureKey,
    ) -> Self {
        match value {
            ConsensusMessage::Proposal(p) => ProcessedConsensusMessage::Proposal(p, sender),
            ConsensusMessage::Vote(v) => ProcessedConsensusMessage::Vote(v, sender),
            ConsensusMessage::InternalTrigger(a) => ProcessedConsensusMessage::InternalTrigger(a),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
/// Messages related to the consensus protocol
pub enum ConsensusMessage<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    PROPOSAL: ProposalType<NodeType = TYPES>,
> {
    /// Leader's proposal
    Proposal(Proposal<PROPOSAL>),
    /// Replica's vote on a proposal.
    Vote(Vote<TYPES, LEAF>),
    /// Internal ONLY message indicating a NextView interrupt
    /// View number this nextview interrupt was generated for
    /// used so we ignore stale nextview interrupts within a task
    #[serde(skip)]
    InternalTrigger(InternalTrigger<TYPES>),
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
    > ConsensusMessage<TYPES, LEAF, PROPOSAL>
{
    /// The view number of the (leader|replica) when the message was sent
    /// or the view of the timeout
    pub fn view_number(&self) -> TYPES::Time {
        match self {
            ConsensusMessage::Proposal(p) => {
                // view of leader in the leaf when proposal
                // this should match replica upon receipt
                p.data.get_view_number()
            }
            ConsensusMessage::Vote(vote_message) => match vote_message {
                Vote::DA(v) => v.current_view,
                Vote::Yes(v) | Vote::No(v) => v.current_view,
                Vote::Timeout(v) => v.current_view,
            },
            ConsensusMessage::InternalTrigger(trigger) => match trigger {
                InternalTrigger::Timeout(time) => *time,
            },
        }
    }
}

#[derive(Serialize, Deserialize, Derivative, Clone, Debug, PartialEq, Eq)]
#[serde(bound(deserialize = ""))]
/// Messages related to sending data between nodes
pub enum DataMessage<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// The newest entry that a node knows. This is send from existing nodes to a new node when the new node joins the network
    NewestQuorumCertificate {
        /// The newest [`QuorumCertificate`]
        quorum_certificate: LEAF::QuorumCertificate,

        /// The relevant [`BlockContents`]
        ///
        /// [`BlockContents`]: ../traits/block_contents/trait.BlockContents.html
        block: TYPES::BlockType,

        /// The relevant [`State`]
        ///
        /// [`State`]: ../traits/state/trait.State.html
        state: LEAF::StateCommitmentType,

        /// The parent leaf's commitment
        parent_commitment: Commitment<LEAF>,

        /// Transactions rejected in this view
        rejected: Vec<TYPES::Transaction>,

        /// the proposer id for this leaf
        proposer_id: EncodedPublicKey,
    },

    /// Contains a transaction to be submitted
    SubmitTransaction(TYPES::Transaction),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(bound(deserialize = ""))]
/// Prepare qc from the leader
pub struct Proposal<PROPOSAL: ProposalType> {
    // NOTE: optimization could include view number to help look up parent leaf
    // could even do 16 bit numbers if we want
    /// The data being proposed.
    pub data: PROPOSAL,
    /// The proposal must be signed by the view leader
    pub signature: EncodedSignature,
}

/// A vote on DA proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct DAVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// TODO we should remove this
    /// this is correct, but highly inefficient
    /// we should check a cache, and if that fails request the qc
    pub justify_qc_commitment: Commitment<LEAF::QuorumCertificate>,
    /// The signature share associated with this vote
    /// TODO ct/vrf make ConsensusMessage generic over I instead of serializing to a Vec<u8>
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The block commitment being voted on.
    pub block_commitment: Commitment<TYPES::BlockType>,
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
}

/// A positive or negative vote on valiadting or commitment proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct YesOrNoVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// TODO we should remove this
    /// this is correct, but highly inefficient
    /// we should check a cache, and if that fails request the qc
    pub justify_qc_commitment: Commitment<LEAF::QuorumCertificate>,
    /// The signature share associated with this vote
    /// TODO ct/vrf make ConsensusMessage generic over I instead of serializing to a Vec<u8>
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The leaf commitment being voted on.
    pub leaf_commitment: Commitment<LEAF>,
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
}

/// A timeout vote.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct TimeoutVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// The justification qc for this view
    pub justify_qc: LEAF::QuorumCertificate,
    /// The signature share associated with this vote
    /// TODO ct/vrf make ConsensusMessage generic over I instead of serializing to a Vec<u8>
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
}
