//! The election trait, used to decide which node is the leader and determine if a vote is valid.

// Needed to avoid the non-biding `let` warning.
#![allow(clippy::let_underscore_untyped)]

use super::node_implementation::{NodeImplementation, NodeType};
use super::signature_key::{EncodedPublicKey, EncodedSignature};
use crate::certificate::{ViewSyncCertificate, AssembledSignature};
use crate::certificate::VoteMetaData;
use crate::certificate::{DACertificate, QuorumCertificate};
use crate::data::DAProposal;
use crate::data::ProposalType;

use crate::message::{
    CommitteeConsensusMessage, GeneralConsensusMessage, Message, SequencingMessage,
};
use crate::vote::ViewSyncVoteInternal;

use crate::traits::network::CommunicationChannel;
use crate::traits::network::NetworkMsg;
use crate::traits::{
    consensus_type::sequencing_consensus::SequencingConsensus,
    node_implementation::{ExchangesType, SequencingExchangesType},
    state::ConsensusTime,
};
use crate::vote::{ViewSyncData, self};
use crate::vote::ViewSyncVote;
use crate::vote::VoteAccumulator;
use crate::vote::{Accumulator, DAVote, QuorumVote, TimeoutVote, VoteType, YesOrNoVote};
use crate::{
    data::{LeafType, SequencingLeaf},
    traits::signature_key::SignatureKey,
};
use bincode::Options;
use commit::{Commitment, Committable};
use derivative::Derivative;
use either::Either;
use hotshot_utils::bincode::bincode_opts;
use serde::Deserialize;
use serde::{de::DeserializeOwned, Serialize};
use snafu::Snafu;
use core::panic;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use tracing::error;
use hotshot_primitives::qc::bit_vector::{BitVectorQC, StakeTableEntry, QCParams};
use hotshot_primitives::qc::QuorumCertificate as AssembledQuorumCertificate;
use jf_primitives::signatures::bls_over_bn254::{BLSOverBN254CurveSignatureScheme, KeyPair as QCKeyPair, VerKey};
use blake3::traits::digest::generic_array::GenericArray;
use ethereum_types::U256;

/// Error for election problems
#[derive(Snafu, Debug)]
pub enum ElectionError {
    /// stub error to be filled in
    StubError,
    /// Math error doing something
    /// NOTE: it would be better to make Election polymorphic over
    /// the election error and then have specific math errors
    MathError,
}

/// For items that will always have the same validity outcome on a successful check,
/// allows for the case of "not yet possible to check" where the check might be
/// attempted again at a later point in time, but saves on repeated checking when
/// the outcome is already knowable.
///
/// This would be a useful general utility.
pub enum Checked<T> {
    /// This item has been checked, and is valid
    Valid(T),
    /// This item has been checked, and is not valid
    Inval(T),
    /// This item has not been checked
    Unchecked(T),
}

/// Data to vote on for different types of votes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub enum VoteData<COMMITTABLE: Committable + Serialize + Clone> {
    /// Vote to provide availability for a block.
    DA(Commitment<COMMITTABLE>),
    /// Vote to append a leaf to the log.
    Yes(Commitment<COMMITTABLE>),
    /// Vote to reject a leaf from the log.
    No(Commitment<COMMITTABLE>),
    /// Vote to time out and proceed to the next view.
    Timeout(Commitment<COMMITTABLE>),
    /// Vote for ViewSync
    ViewSyncPreCommit(Commitment<COMMITTABLE>),
    ViewSyncCommit(Commitment<COMMITTABLE>),
    ViewSyncFinalize(Commitment<COMMITTABLE>),
}

/// Make different types of VoteData committable
impl<COMMITTABLE: Committable + Serialize + Clone> Committable for VoteData<COMMITTABLE> {
    fn commit(&self) -> Commitment<Self> {
        match self {
            VoteData::DA(block_commitment) => 
                commit::RawCommitmentBuilder::new("DA Block Commit")
                    .field("block_commitment", block_commitment.clone())
                    .finalize(),
            VoteData::Yes(leaf_commitment) => 
                commit::RawCommitmentBuilder::new("Yes Vote Commit")
                    .field("leaf_commitment", leaf_commitment.clone())
                    .finalize(),
            VoteData::No(leaf_commitment) =>
                commit::RawCommitmentBuilder::new("No Vote Commit")
                    .field("leaf_commitment", leaf_commitment.clone())
                    .finalize(),
            VoteData::Timeout(view_number_commitment) =>
                commit::RawCommitmentBuilder::new("Timeout View Number Commit")
                    .field("view_number_commitment", view_number_commitment.clone())
                    .finalize(),
            VoteData::ViewSyncPreCommit(commitment) =>
                commit::RawCommitmentBuilder::new("ViewSyncPreCommit")
                    .field("commitment", commitment.clone())
                    .finalize(),
            VoteData::ViewSyncCommit(commitment) =>
                commit::RawCommitmentBuilder::new("ViewSyncCommit")
                    .field("commitment", commitment.clone())
                    .finalize(),
            VoteData::ViewSyncFinalize(commitment) =>
                commit::RawCommitmentBuilder::new("ViewSyncFinalize")
                    .field("commitment", commitment.clone())
                    .finalize(),
        }
    }

    fn tag() -> String {
            ("VOTE_DATA_COMMIT").to_string()
    }
}


impl<COMMITTABLE: Committable + Serialize + Clone> VoteData<COMMITTABLE> {
    #[must_use]
    /// Convert vote data into bytes.
    ///
    /// # Panics
    /// Panics if the serialization fails.
    pub fn as_bytes(&self) -> Vec<u8> {
        bincode_opts().serialize(&self).unwrap()
    }
}

/// Proof of this entity's right to vote, and of the weight of those votes
pub trait VoteToken:
    Clone
    + Debug
    + Send
    + Sync
    + serde::Serialize
    + for<'de> serde::Deserialize<'de>
    + PartialEq
    + Hash
    + Eq
    + Committable
{
    // type StakeTable;
    // type KeyPair: SignatureKey;
    // type ConsensusTime: ConsensusTime;

    /// the count, which validation will confirm
    fn vote_count(&self) -> NonZeroU64;
}

/// election config
pub trait ElectionConfig:
    Default + Clone + Serialize + DeserializeOwned + Sync + Send + core::fmt::Debug
{
}

/// A certificate of some property which has been signed by a quroum of nodes.
pub trait SignedCertificate<SIGNATURE: SignatureKey, TIME, TOKEN, COMMITTABLE>
where
    Self: Send + Sync + Clone + Serialize + for<'a> Deserialize<'a>,
    COMMITTABLE: Committable + Serialize + Clone,
    TOKEN: VoteToken,
{
    /// Build a QC from the threshold signature and commitment
    fn from_signatures_and_commitment(
        view_number: TIME,
        signatures: AssembledSignature,
        commit: Commitment<COMMITTABLE>,
        relay: Option<u64>,
    ) -> Self;

    /// Get the view number.
    fn view_number(&self) -> TIME;

    /// Get signatures.
    fn signatures(&self) -> AssembledSignature;

    // TODO (da) the following functions should be refactored into a QC-specific trait.

    /// Get the leaf commitment.
    fn leaf_commitment(&self) -> Commitment<COMMITTABLE>;

    /// Set the leaf commitment.
    fn set_leaf_commitment(&mut self, commitment: Commitment<COMMITTABLE>);

    /// Get whether the certificate is for the genesis block.
    fn is_genesis(&self) -> bool;

    /// To be used only for generating the genesis quorum certificate; will fail if used anywhere else
    fn genesis() -> Self;
}

/// A protocol for determining membership in and participating in a ccommittee.
pub trait Membership<TYPES: NodeType>:
    Clone + Debug + Eq + PartialEq + Send + Sync + 'static
{
    /// Data used to determine the weight (voting power) of participants.
    type StakeTable: Send + Sync;

    /// generate a default election configuration
    fn default_election_config(num_nodes: u64) -> TYPES::ElectionConfigType;

    /// create an election
    /// TODO may want to move this to a testableelection trait
    fn create_election(entries: Vec<StakeTableEntry<VerKey>>, keys: Vec<TYPES::SignatureKey>, config: TYPES::ElectionConfigType) -> Self;

    /// Returns the table from the current committed state
    fn get_stake_table(
        &self,
        view_number: TYPES::Time,
        state: &TYPES::StateType,
    ) -> Self::StakeTable;

    /// Clone the public key and corresponding stake table for all the nodes
    fn get_qc_stake_table (
        &self
    ) -> Vec<StakeTableEntry<VerKey>>;

    /// Clone the public key and corresponding stake table for current elected committee
    fn get_committee_qc_stake_table (
        &self,
    ) -> Vec<StakeTableEntry<VerKey>>;

    /// The leader of the committee for view `view_number`.
    fn get_leader(&self, view_number: TYPES::Time) -> TYPES::SignatureKey;

    /// The members of the committee for view `view_number`.
    fn get_committee(&self, view_number: TYPES::Time) -> BTreeSet<TYPES::SignatureKey>;

    /// Attempts to generate a vote token for self
    ///
    /// Returns `None` if the number of seats would be zero
    /// # Errors
    /// TODO tbd
    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
        priv_key: &<TYPES::SignatureKey as SignatureKey>::PrivateKey,
        key_pair: QCKeyPair,
    ) -> Result<Option<TYPES::VoteTokenType>, ElectionError>;

    /// Checks the claims of a received vote token
    ///
    /// # Errors
    /// TODO tbd
    fn validate_vote_token(
        &self,
        pub_key: TYPES::SignatureKey,
        token: Checked<TYPES::VoteTokenType>,
    ) -> Result<Checked<TYPES::VoteTokenType>, ElectionError>;

    /// Returns the number of total nodes in the committee
    fn total_nodes(&self) -> usize;

    /// Returns the threshold for a specific `Membership` implementation
    fn success_threshold(&self) -> NonZeroU64;

    /// Returns the threshold for a specific `Membership` implementation
    fn failure_threshold(&self) -> NonZeroU64;
}

/// Protocol for exchanging proposals and votes to make decisions in a distributed network.
///
/// An instance of [`ConsensusExchange`] represents the state of one participant in the protocol,
/// allowing them to vote and query information about the overall state of the protocol (such as
/// membership and leader status).
pub trait ConsensusExchange<TYPES: NodeType, M: NetworkMsg>: Send + Sync {
    /// A proposal for participants to vote on.
    type Proposal: ProposalType<NodeType = TYPES>;
    /// A vote on a [`Proposal`](Self::Proposal).
    type Vote: VoteType<TYPES>;
    /// A [`SignedCertificate`] attesting to a decision taken by the committee.
    type Certificate: SignedCertificate<TYPES::SignatureKey, TYPES::Time, TYPES::VoteTokenType, Self::Commitment>
        + Hash
        + Eq;
    /// The committee eligible to make decisions.
    type Membership: Membership<TYPES>;
    /// Network used by [`Membership`](Self::Membership) to communicate.
    type Networking: CommunicationChannel<TYPES, M, Self::Proposal, Self::Vote, Self::Membership>;
    /// Commitments to items which are the subject of proposals and decisions.
    type Commitment: Committable + Serialize + Clone;

    /// Join a [`ConsensusExchange`] with the given identity (`pk` and `sk`).
    fn create(
        entries: Vec<StakeTableEntry<VerKey>>,
        keys: Vec<TYPES::SignatureKey>,
        config: TYPES::ElectionConfigType,
        network: Self::Networking,
        pk: TYPES::SignatureKey,
        key_pair: QCKeyPair,
        entry: StakeTableEntry<VerKey>,
        sk: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    ) -> Self;

    /// The network being used by this exchange.
    fn network(&self) -> &Self::Networking;

    /// The leader of the [`Membership`](Self::Membership) at time `view_number`.
    fn get_leader(&self, view_number: TYPES::Time) -> TYPES::SignatureKey {
        self.membership().get_leader(view_number)
    }

    /// Whether this participant is leader at time `view_number`.
    fn is_leader(&self, view_number: TYPES::Time) -> bool {
        &self.get_leader(view_number) == self.public_key()
    }

    /// Threshold required to approve a [`Proposal`](Self::Proposal).
    fn success_threshold(&self) -> NonZeroU64 {
        self.membership().success_threshold()
    }

    /// Threshold required to know a success threshold will not be reached
    fn failure_threshold(&self) -> NonZeroU64 {
        self.membership().failure_threshold()
    }

    /// The total number of nodes in the committee.
    fn total_nodes(&self) -> usize {
        self.membership().total_nodes()
    }

    /// Attempts to generate a vote token for participation at time `view_number`.
    ///
    /// # Errors
    /// When unable to make a vote token because not part of the committee
    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
    ) -> std::result::Result<std::option::Option<TYPES::VoteTokenType>, ElectionError> {
        self.membership()
            .make_vote_token(view_number, self.private_key(), (*self.key_pair()).clone())
    }

    /// The contents of a vote on `commit`.
    fn vote_data(&self, commit: Commitment<Self::Commitment>) -> VoteData<Self::Commitment>;

    /// Validate a QC.
    fn is_valid_cert(&self, qc: &Self::Certificate, commit: Commitment<Self::Commitment>) -> bool {
        
        if qc.is_genesis() && qc.view_number() == TYPES::Time::genesis() {
            return true;
        }
        let leaf_commitment = qc.leaf_commitment();

        if leaf_commitment != commit {
            error!("Leaf commitment does not equal parent commitment");
            return false;
        }

        match qc.signatures() {
            AssembledSignature::DA(qc) => {
                let real_commit = VoteData::DA(leaf_commitment).commit();
                let msg = GenericArray::from_slice(real_commit.as_ref());
                let real_qc_pp = QCParams {
                    stake_entries: self.membership().get_committee_qc_stake_table(), 
                    threshold: U256::from(self.membership().success_threshold().get()),
                    agg_sig_pp: (),
                };
                BitVectorQC::<BLSOverBN254CurveSignatureScheme>::check(
                    &real_qc_pp, 
                    &msg,
                    &qc
                ).is_ok()
            }
            AssembledSignature::Yes(qc) => {
                let real_commit = VoteData::Yes(leaf_commitment).commit();
                let msg = GenericArray::from_slice(real_commit.as_ref());
                let real_qc_pp = QCParams {
                    stake_entries: self.membership().get_committee_qc_stake_table(), 
                    threshold: U256::from(self.membership().success_threshold().get()),
                    agg_sig_pp: (),
                };
                BitVectorQC::<BLSOverBN254CurveSignatureScheme>::check(
                    &real_qc_pp, 
                    &msg,
                    &qc
                ).is_ok()
            }
            AssembledSignature::No(qc) => {
                let real_commit = VoteData::No(leaf_commitment).commit();
                let msg = GenericArray::from_slice(real_commit.as_ref());
                let real_qc_pp = QCParams {
                    stake_entries: self.membership().get_committee_qc_stake_table(), 
                    threshold: U256::from(self.membership().success_threshold().get()),
                    agg_sig_pp: (),
                };
                BitVectorQC::<BLSOverBN254CurveSignatureScheme>::check(
                    &real_qc_pp, 
                    &msg,
                    &qc
                ).is_ok()
            }
            AssembledSignature::Genesis() => {
                return true;
            }
            AssembledSignature::ViewSyncPreCommit(_)
            | AssembledSignature::ViewSyncCommit(_)
            | AssembledSignature::ViewSyncFinalize(_) => {
                panic!("QC should not be ViewSync type here");
            }
        }
    }

    /// Validate a vote by checking its signature and token.
    fn is_valid_vote(
        &self,
        ver_key: VerKey,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        data: VoteData<Self::Commitment>,
        view_number: TYPES::Time,
        vote_token: Checked<TYPES::VoteTokenType>,
    ) -> bool {
        let mut is_valid_vote_token = false;
        let mut is_valid_signature = false;
        if let Some(key) = <TYPES::SignatureKey as SignatureKey>::from_bytes(encoded_key) {
            is_valid_signature = key.validate(encoded_signature, &data.commit().as_ref());
            let valid_vote_token =
                self.membership()
                    .validate_vote_token(key, vote_token);
            is_valid_vote_token = match valid_vote_token {
                Err(_) => {
                    error!("Vote token was invalid");
                    false
                }
                Ok(Checked::Valid(_)) => true,
                Ok(Checked::Inval(_) | Checked::Unchecked(_)) => false,
            };
        }
        is_valid_signature && is_valid_vote_token
    }

    #[doc(hidden)]
    fn accumulate_internal(
        &self,
        vota_meta: VoteMetaData<Self::Commitment, TYPES::VoteTokenType, TYPES::Time>,
        accumulator: VoteAccumulator<TYPES::VoteTokenType, Self::Commitment>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, Self::Commitment>, Self::Certificate> {
        if !self.is_valid_vote(
            vota_meta.ver_key,
            &vota_meta.encoded_key,
            &vota_meta.encoded_signature,
            vota_meta.data.clone(),
            vota_meta.view_number,
            // Ignoring deserialization errors below since we are getting rid of it soon
            Checked::Unchecked(vota_meta.vote_token.clone()),
        ) {
            error!("Invalid vote!");
            return Either::Left(accumulator);
        }

        let append_node_id = self.membership().get_committee_qc_stake_table().iter().position(|x| *x == vota_meta.entry.clone()).unwrap();

        match accumulator.append((
            vota_meta.commitment,
            (
                vota_meta.encoded_key.clone(),
                (
                    vota_meta.encoded_signature.clone(),
                    self.membership().get_committee_qc_stake_table(),
                    append_node_id,
                    vota_meta.data,
                    vota_meta.vote_token,
                ),
            ),
        )) {
            Either::Left(accumulator) => Either::Left(accumulator),
            Either::Right(signatures) => {
                Either::Right(Self::Certificate::from_signatures_and_commitment(
                    vota_meta.view_number,
                    // agg_signatures,
                    signatures,
                    vota_meta.commitment,
                    vota_meta.relay,
                ))
            }
        }
    }

    /// Add a vote to the accumulating signature.  Return The certificate if the vote
    /// brings us over the threshould, Else return the accumulator.
    #[allow(clippy::too_many_arguments)]
    fn accumulate_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        entry: StakeTableEntry<VerKey>,
        leaf_commitment: Commitment<Self::Commitment>,
        vote_data: VoteData<Self::Commitment>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, Self::Commitment>,
        relay: Option<u64>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, Self::Commitment>, Self::Certificate>;

    /// The committee which votes on proposals.
    fn membership(&self) -> &Self::Membership;

    /// This participant's public key.
    fn public_key(&self) -> &TYPES::SignatureKey;

    /// This participant's private key.
    fn private_key(&self) -> &<TYPES::SignatureKey as SignatureKey>::PrivateKey;

    /// Get KeyPair for signature scheme in certificate Aggregation
    fn key_pair(&self) -> &QCKeyPair;
}

/// A [`ConsensusExchange`] where participants vote to provide availability for blobs of data.
pub trait CommitteeExchangeType<TYPES: NodeType<ConsensusType = SequencingConsensus>, M: NetworkMsg>:
    ConsensusExchange<TYPES, M>
{
    /// Sign a DA proposal.
    fn sign_da_proposal(&self, block_commitment: &Commitment<TYPES::BlockType>)
        -> (EncodedSignature, VerKey);

    /// Sign a vote on DA proposal.
    ///
    /// The block commitment and the type of the vote (DA) are signed, which is the minimum amount
    /// of information necessary for checking that this node voted on that block.
    fn sign_da_vote(
        &self,
        block_commitment: Commitment<TYPES::BlockType>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    /// Create a message with a vote on DA proposal.
    fn create_da_message(
        &self,
        block_commitment: Commitment<TYPES::BlockType>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> CommitteeConsensusMessage<TYPES>;
}

/// Standard implementation of [`CommitteeExchangeType`] utilizing a DA committee.
#[derive(Derivative)]
#[derivative(Clone, Debug)]
pub struct CommitteeExchange<
    TYPES: NodeType<ConsensusType = SequencingConsensus>,
    MEMBERSHIP: Membership<TYPES>,
    NETWORK: CommunicationChannel<TYPES, M, DAProposal<TYPES>, DAVote<TYPES>, MEMBERSHIP>,
    M: NetworkMsg,
> {
    /// The network being used by this exchange.
    network: NETWORK,
    /// The committee which votes on proposals.
    membership: MEMBERSHIP,
    /// This participant's public key.
    public_key: TYPES::SignatureKey,
    /// KeyPair with signature scheme for certificate aggregation,
    key_pair: QCKeyPair,
    /// Entry with public key and staking value for certificate aggregation
    entry: StakeTableEntry<VerKey>,
    /// This participant's private key.
    #[derivative(Debug = "ignore")]
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    #[doc(hidden)]
    _pd: PhantomData<(TYPES, MEMBERSHIP, M)>,
}

impl<
        TYPES: NodeType<ConsensusType = SequencingConsensus>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, DAProposal<TYPES>, DAVote<TYPES>, MEMBERSHIP>,
        M: NetworkMsg,
    > CommitteeExchangeType<TYPES, M> for CommitteeExchange<TYPES, MEMBERSHIP, NETWORK, M>
{
    /// Sign a DA proposal.
    fn sign_da_proposal(
        &self,
        block_commitment: &Commitment<TYPES::BlockType>,
    ) -> (EncodedSignature, VerKey) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(), 
            block_commitment.as_ref()
        );
        (signature, self.key_pair.ver_key())
    }
    /// Sign a vote on DA proposal.
    ///
    /// The block commitment and the type of the vote (DA) are signed, which is the minimum amount
    /// of information necessary for checking that this node voted on that block.
    fn sign_da_vote(
        &self,
        block_commitment: Commitment<TYPES::BlockType>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::<TYPES::BlockType>::DA(block_commitment).commit().as_ref(),
        );
        (self.public_key.to_bytes(), signature, self.entry.clone())
    }
    /// Create a message with a vote on DA proposal.
    fn create_da_message(
        &self,
        block_commitment: Commitment<TYPES::BlockType>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> CommitteeConsensusMessage<TYPES> {
        let signature = self.sign_da_vote(block_commitment);
        CommitteeConsensusMessage::<TYPES>::DAVote(DAVote {
            signature,
            block_commitment,
            current_view,
            vote_token,
            vote_data: VoteData::DA(block_commitment),
        })
    }
}

impl<
        TYPES: NodeType<ConsensusType = SequencingConsensus>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, DAProposal<TYPES>, DAVote<TYPES>, MEMBERSHIP>,
        M: NetworkMsg,
    > ConsensusExchange<TYPES, M> for CommitteeExchange<TYPES, MEMBERSHIP, NETWORK, M>
{
    type Proposal = DAProposal<TYPES>;
    type Vote = DAVote<TYPES>;
    type Certificate = DACertificate<TYPES>;
    type Membership = MEMBERSHIP;
    type Networking = NETWORK;
    type Commitment = TYPES::BlockType;

    fn create(
        entries: Vec<StakeTableEntry<VerKey>>,
        keys: Vec<TYPES::SignatureKey>,
        config: TYPES::ElectionConfigType,
        network: Self::Networking,
        pk: TYPES::SignatureKey,
        key_pair: QCKeyPair,
        entry: StakeTableEntry<VerKey>,
        sk: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    ) -> Self {
        let membership =
            <Self as ConsensusExchange<TYPES, M>>::Membership::create_election(entries, keys, config);
        Self {
            network,
            membership,
            public_key: pk,
            key_pair,
            entry,
            private_key: sk,
            _pd: PhantomData,
        }
    }
    fn network(&self) -> &NETWORK {
        &self.network
    }
    fn make_vote_token(
        &self,
        view_number: TYPES::Time,
    ) -> std::result::Result<std::option::Option<TYPES::VoteTokenType>, ElectionError> {
        self.membership
            .make_vote_token(view_number, &self.private_key, self.key_pair.clone())
    }

    fn vote_data(&self, commit: Commitment<Self::Commitment>) -> VoteData<Self::Commitment> {
        VoteData::DA(commit)
    }

    /// Add a vote to the accumulating signature.  Return The certificate if the vote
    /// brings us over the threshould, Else return the accumulator.
    fn accumulate_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        entry: StakeTableEntry<VerKey>,
        leaf_commitment: Commitment<Self::Commitment>,
        vote_data: VoteData<Self::Commitment>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, Self::Commitment>,
        relay: Option<u64>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, Self::Commitment>, Self::Certificate> {
        let meta = VoteMetaData {
            encoded_key: encoded_key.clone(),
            encoded_signature: encoded_signature.clone(),
            entry: entry.clone(),
            ver_key: entry.stake_key,
            commitment: leaf_commitment,
            data: vote_data,
            vote_token,
            view_number,
            relay: None,
        };
        self.accumulate_internal(meta, accumlator)
    }
    fn membership(&self) -> &Self::Membership {
        &self.membership
    }
    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.public_key
    }
    fn private_key(&self) -> &<<TYPES as NodeType>::SignatureKey as SignatureKey>::PrivateKey {
        &self.private_key
    }
    fn key_pair(&self) -> &QCKeyPair {
        &self.key_pair
    }
}

/// A [`ConsensusExchange`] where participants vote to append items to a log.
pub trait QuorumExchangeType<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>, M: NetworkMsg>:
    ConsensusExchange<TYPES, M>
{
    /// Create a message with a positive vote on validating or commitment proposal.
    fn create_yes_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<Self::Certificate>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>
    where
        <Self as ConsensusExchange<TYPES, M>>::Certificate: commit::Committable,
        I::Exchanges: ExchangesType<TYPES::ConsensusType, TYPES, LEAF, Message<TYPES, I>>;

    /// Sign a validating or commitment proposal.
    fn sign_validating_or_commitment_proposal<I: NodeImplementation<TYPES>>(
        &self,
        leaf_commitment: &Commitment<LEAF>,
    ) -> (EncodedSignature, VerKey);

    /// Sign a positive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (yes) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `Yes` on that leaf. The leaf is expected to be reconstructed based on other
    /// information in the yes vote.
    fn sign_yes_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    /// Sign a neagtive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (no) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `No` on that leaf.
    fn sign_no_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    /// Sign a timeout vote.
    ///
    /// We only sign the view number, which is the minimum amount of information necessary for
    /// checking that this node timed out on that view.
    ///
    /// This also allows for the high QC included with the vote to be spoofed in a MITM scenario,
    /// but it is outside our threat model.
    fn sign_timeout_vote(&self, view_number: TYPES::Time) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    /// Create a message with a negative vote on validating or commitment proposal.
    fn create_no_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>
    where
        I::Exchanges: ExchangesType<TYPES::ConsensusType, TYPES, I::Leaf, Message<TYPES, I>>;

    /// Create a message with a timeout vote on validating or commitment proposal.
    fn create_timeout_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc: QuorumCertificate<TYPES, LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>
    where
        I::Exchanges: ExchangesType<TYPES::ConsensusType, TYPES, I::Leaf, Message<TYPES, I>>;
}

/// Standard implementation of [`QuroumExchangeType`] based on Hot Stuff consensus.
#[derive(Derivative)]
#[derivative(Clone, Debug)]
pub struct QuorumExchange<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    PROPOSAL: ProposalType<NodeType = TYPES>,
    MEMBERSHIP: Membership<TYPES>,
    NETWORK: CommunicationChannel<TYPES, M, PROPOSAL, QuorumVote<TYPES, LEAF>, MEMBERSHIP>,
    M: NetworkMsg,
> {
    /// The network being used by this exchange.
    network: NETWORK,
    /// The committee which votes on proposals.
    membership: MEMBERSHIP,
    /// This participant's public key.
    public_key: TYPES::SignatureKey,
    /// KeyPair with Signature Scheme for certificate aggregation,
    key_pair: QCKeyPair,
    /// Entry with public key and staking value for certificate aggregation
    entry: StakeTableEntry<VerKey>,
    /// This participant's private key.
    #[derivative(Debug = "ignore")]
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    #[doc(hidden)]
    _pd: PhantomData<(LEAF, PROPOSAL, MEMBERSHIP, M)>,
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, PROPOSAL, QuorumVote<TYPES, LEAF>, MEMBERSHIP>,
        M: NetworkMsg,
    > QuorumExchangeType<TYPES, LEAF, M>
    for QuorumExchange<TYPES, LEAF, PROPOSAL, MEMBERSHIP, NETWORK, M>
{
    /// Create a message with a positive vote on validating or commitment proposal.
    fn create_yes_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>
    where
        I::Exchanges: ExchangesType<TYPES::ConsensusType, TYPES, LEAF, Message<TYPES, I>>,
    {
        let signature = self.sign_yes_vote(leaf_commitment);
        GeneralConsensusMessage::<TYPES, I>::Vote(QuorumVote::Yes(YesOrNoVote {
            justify_qc_commitment,
            signature,
            leaf_commitment,
            current_view,
            vote_token,
            vote_data: VoteData::Yes(leaf_commitment),
        }))
    }
    /// Sign a validating or commitment proposal.
    fn sign_validating_or_commitment_proposal<I: NodeImplementation<TYPES>>(
        &self,
        leaf_commitment: &Commitment<LEAF>,
    ) -> (EncodedSignature, VerKey) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(), 
            leaf_commitment.as_ref()
        );
        (signature, self.key_pair.ver_key().clone())
    }

    /// Sign a positive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (yes) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `Yes` on that leaf. The leaf is expected to be reconstructed based on other
    /// information in the yes vote.
    fn sign_yes_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::<LEAF>::Yes(leaf_commitment).commit().as_ref(),
        );
        (self.public_key.to_bytes(), signature, self.entry.clone())
    }

    /// Sign a neagtive vote on validating or commitment proposal.
    ///
    /// The leaf commitment and the type of the vote (no) are signed, which is the minimum amount
    /// of information necessary for any user of the subsequently constructed QC to check that this
    /// node voted `No` on that leaf.
    fn sign_no_vote(
        &self,
        leaf_commitment: Commitment<LEAF>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::<LEAF>::No(leaf_commitment).commit().as_ref(),
        );
        (self.public_key.to_bytes(), signature, self.entry.clone())
    }

    /// Sign a timeout vote.
    ///
    /// We only sign the view number, which is the minimum amount of information necessary for
    /// checking that this node timed out on that view.
    ///
    /// This also allows for the high QC included with the vote to be spoofed in a MITM scenario,
    /// but it is outside our threat model.
    fn sign_timeout_vote(&self, view_number: TYPES::Time) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::<TYPES::Time>::Timeout(view_number.commit()).commit().as_ref(),
        );
        (self.public_key.to_bytes(), signature, self.entry.clone())
    }
    /// Create a message with a negative vote on validating or commitment proposal.
    fn create_no_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
        leaf_commitment: Commitment<LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>
    where
        I::Exchanges: ExchangesType<TYPES::ConsensusType, TYPES, LEAF, Message<TYPES, I>>,
    {
        let signature = self.sign_no_vote(leaf_commitment);
        GeneralConsensusMessage::<TYPES, I>::Vote(QuorumVote::No(YesOrNoVote {
            justify_qc_commitment,
            signature,
            leaf_commitment,
            current_view,
            vote_token,
            vote_data: VoteData::No(leaf_commitment),
        }))
    }

    /// Create a message with a timeout vote on validating or commitment proposal.
    fn create_timeout_message<I: NodeImplementation<TYPES, Leaf = LEAF>>(
        &self,
        justify_qc: QuorumCertificate<TYPES, LEAF>,
        current_view: TYPES::Time,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>
    where
        I::Exchanges: ExchangesType<TYPES::ConsensusType, TYPES, I::Leaf, Message<TYPES, I>>,
    {
        let signature = self.sign_timeout_vote(current_view);
        GeneralConsensusMessage::<TYPES, I>::Vote(QuorumVote::Timeout(TimeoutVote {
            justify_qc,
            signature,
            current_view,
            vote_token,
            vote_data: VoteData::Timeout(current_view.commit()),
        }))
    }
}

impl<
        TYPES: NodeType,
        LEAF: LeafType<NodeType = TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, PROPOSAL, QuorumVote<TYPES, LEAF>, MEMBERSHIP>,
        M: NetworkMsg,
    > ConsensusExchange<TYPES, M>
    for QuorumExchange<TYPES, LEAF, PROPOSAL, MEMBERSHIP, NETWORK, M>
{
    type Proposal = PROPOSAL;
    type Vote = QuorumVote<TYPES, LEAF>;
    type Certificate = QuorumCertificate<TYPES, LEAF>;
    type Membership = MEMBERSHIP;
    type Networking = NETWORK;
    type Commitment = LEAF;

    fn create(
        entries: Vec<StakeTableEntry<VerKey>>,
        keys: Vec<TYPES::SignatureKey>,
        config: TYPES::ElectionConfigType,
        network: Self::Networking,
        pk: TYPES::SignatureKey,
        key_pair: QCKeyPair,
        entry: StakeTableEntry<VerKey>,
        sk: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    ) -> Self {
        let membership =
            <Self as ConsensusExchange<TYPES, M>>::Membership::create_election(entries, keys, config);
        Self {
            network,
            membership,
            public_key: pk,
            key_pair,
            entry,
            private_key: sk,
            _pd: PhantomData,
        }
    }

    fn network(&self) -> &NETWORK {
        &self.network
    }

    fn vote_data(&self, commit: Commitment<Self::Commitment>) -> VoteData<Self::Commitment> {
        VoteData::Yes(commit)
    }

    /// Add a vote to the accumulating signature.  Return The certificate if the vote
    /// brings us over the threshould, Else return the accumulator.
    fn accumulate_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        entry: StakeTableEntry<VerKey>,
        leaf_commitment: Commitment<LEAF>,
        vote_data: VoteData<Self::Commitment>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, LEAF>,
        relay: Option<u64>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, LEAF>, Self::Certificate> {
        let meta = VoteMetaData {
            encoded_key: encoded_key.clone(),
            encoded_signature: encoded_signature.clone(),
            entry: entry.clone(),
            ver_key: entry.stake_key,
            commitment: leaf_commitment,
            data: vote_data,
            vote_token,
            view_number,
            relay: None,
        };
        self.accumulate_internal(meta, accumlator)
    }
    fn membership(&self) -> &Self::Membership {
        &self.membership
    }
    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.public_key
    }
    fn private_key(&self) -> &<<TYPES as NodeType>::SignatureKey as SignatureKey>::PrivateKey {
        &self.private_key
    }
    fn key_pair(&self) -> &QCKeyPair {
        &self.key_pair
    }
}

/// A [`ConsensusExchange`] where participants synchronize which view the network should be in.
pub trait ViewSyncExchangeType<TYPES: NodeType, M: NetworkMsg>:
    ConsensusExchange<TYPES, M>
{
    /// Creates a precommit vote
    fn create_precommit_message<I: NodeImplementation<TYPES>>(
        &self,
        round: TYPES::Time,
        relay: u64,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>;

    /// Signs a precommit vote
    fn sign_precommit_message(
        &self,
        commitment: Commitment<ViewSyncData<TYPES>>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    /// Creates a commit vote
    fn create_commit_message<I: NodeImplementation<TYPES>>(
        &self,
        round: TYPES::Time,
        relay: u64,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>;

    /// Signs a commit vote
    fn sign_commit_message(
        &self,
        commitment: Commitment<ViewSyncData<TYPES>>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    /// Creates a finalize vote
    fn create_finalize_message<I: NodeImplementation<TYPES>>(
        &self,
        round: TYPES::Time,
        relay: u64,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I>;

    /// Sings a finalize vote
    fn sign_finalize_message(
        &self,
        commitment: Commitment<ViewSyncData<TYPES>>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>);

    fn is_valid_view_sync_cert(&self, certificate: Self::Certificate, round: TYPES::Time) -> bool;

    fn sign_certificate_proposal(&self, certificate: Self::Certificate) -> (EncodedSignature, VerKey);
}

/// Standard implementation of [`ViewSyncExchangeType`] based on Hot Stuff consensus.
#[derive(Derivative)]
#[derivative(Clone, Debug)]
pub struct ViewSyncExchange<
    TYPES: NodeType,
    PROPOSAL: ProposalType<NodeType = TYPES>,
    MEMBERSHIP: Membership<TYPES>,
    NETWORK: CommunicationChannel<TYPES, M, PROPOSAL, ViewSyncVote<TYPES>, MEMBERSHIP>,
    M: NetworkMsg,
> {
    /// The network being used by this exchange.
    network: NETWORK,
    /// The committee which votes on proposals.
    membership: MEMBERSHIP,
    /// This participant's public key.
    public_key: TYPES::SignatureKey,
    /// KeyPair with signature scheme for certificate aggregation,
    key_pair: QCKeyPair,
    /// Entry with public key and staking value for certificate aggregation in the stake table.
    entry: StakeTableEntry<VerKey>,
    /// This participant's private key.
    #[derivative(Debug = "ignore")]
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    #[doc(hidden)]
    _pd: PhantomData<(PROPOSAL, MEMBERSHIP, M)>,
}

impl<
        TYPES: NodeType,
        MEMBERSHIP: Membership<TYPES>,
        PROPOSAL: ProposalType<NodeType = TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, PROPOSAL, ViewSyncVote<TYPES>, MEMBERSHIP>,
        M: NetworkMsg,
    > ViewSyncExchangeType<TYPES, M> for ViewSyncExchange<TYPES, PROPOSAL, MEMBERSHIP, NETWORK, M>
{
    fn create_precommit_message<I: NodeImplementation<TYPES>>(
        &self,
        round: TYPES::Time,
        relay: u64,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I> {
        let relay_pub_key = self.get_leader(round + relay).to_bytes();

        let vote_data_internal: ViewSyncData<TYPES> = ViewSyncData {
            relay: relay_pub_key.clone(),
            round,
        };

        let vote_data_internal_commitment = vote_data_internal.commit();

        let signature = self.sign_precommit_message(vote_data_internal_commitment);

        GeneralConsensusMessage::<TYPES, I>::ViewSyncVote(ViewSyncVote::PreCommit(
            ViewSyncVoteInternal {
                relay_pub_key,
                relay,
                round,
                signature,
                vote_token,
                vote_data: VoteData::ViewSyncPreCommit(vote_data_internal_commitment),
            },
        ))
    }

    fn sign_precommit_message(
        &self,
        commitment: Commitment<ViewSyncData<TYPES>>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::ViewSyncPreCommit(commitment).commit().as_ref(),
        );

        (self.public_key.to_bytes(), signature, self.entry.clone())
    }

    fn create_commit_message<I: NodeImplementation<TYPES>>(
        &self,
        round: TYPES::Time,
        relay: u64,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I> {
        let relay_pub_key = self.get_leader(round + relay).to_bytes();

        let vote_data_internal: ViewSyncData<TYPES> = ViewSyncData {
            relay: relay_pub_key.clone(),
            round,
        };

        let vote_data_internal_commitment = vote_data_internal.commit();

        let signature = self.sign_commit_message(vote_data_internal_commitment);

        GeneralConsensusMessage::<TYPES, I>::ViewSyncVote(ViewSyncVote::Commit(
            ViewSyncVoteInternal {
                relay_pub_key,
                relay,
                round,
                signature,
                vote_token,
                vote_data: VoteData::ViewSyncCommit(vote_data_internal_commitment),
            },
        ))
    }

    fn sign_commit_message(
        &self,
        commitment: Commitment<ViewSyncData<TYPES>>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::ViewSyncCommit(commitment).commit().as_ref(),
        );

        (self.public_key.to_bytes(), signature, self.entry.clone())
    }

    fn create_finalize_message<I: NodeImplementation<TYPES>>(
        &self,
        round: TYPES::Time,
        relay: u64,
        vote_token: TYPES::VoteTokenType,
    ) -> GeneralConsensusMessage<TYPES, I> {
        let relay_pub_key = self.get_leader(round + relay).to_bytes();

        let vote_data_internal: ViewSyncData<TYPES> = ViewSyncData {
            relay: relay_pub_key.clone(),
            round,
        };

        let vote_data_internal_commitment = vote_data_internal.commit();

        let signature = self.sign_finalize_message(vote_data_internal_commitment);

        GeneralConsensusMessage::<TYPES, I>::ViewSyncVote(ViewSyncVote::Finalize(
            ViewSyncVoteInternal {
                relay_pub_key,
                relay,
                round,
                signature,
                vote_token,
                vote_data: VoteData::ViewSyncFinalize(vote_data_internal_commitment),
            },
        ))
    }

    fn sign_finalize_message(
        &self,
        commitment: Commitment<ViewSyncData<TYPES>>,
    ) -> (EncodedPublicKey, EncodedSignature, StakeTableEntry<VerKey>) {
        let signature = TYPES::SignatureKey::sign(
            self.key_pair.clone(),
            &VoteData::ViewSyncFinalize(commitment).commit().as_ref(),
        );

        (self.public_key.to_bytes(), signature, self.entry.clone())
    }

    
    fn is_valid_view_sync_cert(&self, certificate: Self::Certificate, round: TYPES::Time) -> bool {
        // Sishan NOTE TODO: would be better to test this, looks like this func is never called.
        let (certificate_internal, threshold, vote_data) = match certificate.clone() {
            ViewSyncCertificate::PreCommit(certificate_internal) => {
                let vote_data = 
                    ViewSyncData::<TYPES> {
                        relay: self
                            .get_leader(round + certificate_internal.relay)
                            .to_bytes(),
                        round,
                    };
                (certificate_internal, self.failure_threshold(), vote_data)
            }
            ViewSyncCertificate::Commit(certificate_internal) => {
                let vote_data = 
                    ViewSyncData::<TYPES> {
                        relay: self
                            .get_leader(round + certificate_internal.relay)
                            .to_bytes(),
                        round,
                    };
                (certificate_internal, self.success_threshold(), vote_data)
            }
            ViewSyncCertificate::Finalize(certificate_internal) => {
                let vote_data = 
                    ViewSyncData::<TYPES> {
                        relay: self
                            .get_leader(round + certificate_internal.relay)
                            .to_bytes(),
                        round,
                    };
                (certificate_internal, self.success_threshold(), vote_data)
            }
        };
        match certificate_internal.signatures {
            AssembledSignature::ViewSyncPreCommit(raw_signatures) => {
                let real_commit = VoteData::ViewSyncPreCommit(vote_data.commit()).commit();
                let msg = GenericArray::from_slice(real_commit.as_ref());
                let real_qc_pp = QCParams {
                    stake_entries: self.membership().get_committee_qc_stake_table(), 
                    threshold: U256::from(self.membership().success_threshold().get()),
                    agg_sig_pp: (),
                };
                BitVectorQC::<BLSOverBN254CurveSignatureScheme>::check(
                    &real_qc_pp, 
                    &msg,
                    &raw_signatures
                ).is_ok()
            }
            AssembledSignature::ViewSyncCommit(raw_signatures) => {
                let real_commit = VoteData::ViewSyncCommit(vote_data.commit()).commit();
                let msg = GenericArray::from_slice(real_commit.as_ref());
                let real_qc_pp = QCParams {
                    stake_entries: self.membership().get_committee_qc_stake_table(), 
                    threshold: U256::from(self.membership().success_threshold().get()),
                    agg_sig_pp: (),
                };
                BitVectorQC::<BLSOverBN254CurveSignatureScheme>::check(
                    &real_qc_pp, 
                    &msg,
                    &raw_signatures
                ).is_ok()
            }
            AssembledSignature::ViewSyncFinalize(raw_signatures) => {
                let real_commit = VoteData::ViewSyncFinalize(vote_data.commit()).commit();
                let msg = GenericArray::from_slice(real_commit.as_ref());
                let real_qc_pp = QCParams {
                    stake_entries: self.membership().get_committee_qc_stake_table(), 
                    threshold: U256::from(self.membership().success_threshold().get()),
                    agg_sig_pp: (),
                };
                BitVectorQC::<BLSOverBN254CurveSignatureScheme>::check(
                    &real_qc_pp, 
                    &msg,
                    &raw_signatures
                ).is_ok()
            }
            _ => true,
        }
    }

    fn sign_certificate_proposal(&self, certificate: Self::Certificate) -> (EncodedSignature, VerKey) {
        let signature = TYPES::SignatureKey::sign(self.key_pair.clone(), &certificate.commit().as_ref());
        ( signature, self.key_pair.ver_key().clone())
    }
}

impl<
        TYPES: NodeType,
        PROPOSAL: ProposalType<NodeType = TYPES>,
        MEMBERSHIP: Membership<TYPES>,
        NETWORK: CommunicationChannel<TYPES, M, PROPOSAL, ViewSyncVote<TYPES>, MEMBERSHIP>,
        M: NetworkMsg,
    > ConsensusExchange<TYPES, M> for ViewSyncExchange<TYPES, PROPOSAL, MEMBERSHIP, NETWORK, M>
{
    type Proposal = PROPOSAL;
    type Vote = ViewSyncVote<TYPES>;
    type Certificate = ViewSyncCertificate<TYPES>;
    type Membership = MEMBERSHIP;
    type Networking = NETWORK;
    type Commitment = ViewSyncData<TYPES>;

    fn create(
        entries: Vec<StakeTableEntry<VerKey>>,
        keys: Vec<TYPES::SignatureKey>,
        config: TYPES::ElectionConfigType,
        network: Self::Networking,
        pk: TYPES::SignatureKey,
        key_pair: QCKeyPair,
        entry: StakeTableEntry<VerKey>,
        sk: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
    ) -> Self {
        let membership =
            <Self as ConsensusExchange<TYPES, M>>::Membership::create_election(entries, keys, config);
        Self {
            network,
            membership,
            public_key: pk,
            key_pair,
            entry,
            private_key: sk,
            _pd: PhantomData,
        }
    }

    fn network(&self) -> &NETWORK {
        &self.network
    }

    fn vote_data(&self, _commit: Commitment<Self::Commitment>) -> VoteData<Self::Commitment> {
        unimplemented!()
    }

    fn accumulate_vote(
        &self,
        encoded_key: &EncodedPublicKey,
        encoded_signature: &EncodedSignature,
        entry: StakeTableEntry<VerKey>,
        leaf_commitment: Commitment<ViewSyncData<TYPES>>,
        vote_data: VoteData<Self::Commitment>,
        vote_token: TYPES::VoteTokenType,
        view_number: TYPES::Time,
        accumlator: VoteAccumulator<TYPES::VoteTokenType, ViewSyncData<TYPES>>,
        relay: Option<u64>,
    ) -> Either<VoteAccumulator<TYPES::VoteTokenType, ViewSyncData<TYPES>>, Self::Certificate> {
        let meta = VoteMetaData {
            encoded_key: encoded_key.clone(),
            encoded_signature: encoded_signature.clone(),
            entry: entry.clone(),
            ver_key: entry.stake_key,
            commitment: leaf_commitment,
            data: vote_data,
            vote_token,
            view_number,
            relay,
        };
        self.accumulate_internal(meta, accumlator)
    }

    fn membership(&self) -> &Self::Membership {
        &self.membership
    }
    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.public_key
    }
    fn private_key(&self) -> &<<TYPES as NodeType>::SignatureKey as SignatureKey>::PrivateKey {
        &self.private_key
    }
    fn key_pair(&self) -> &QCKeyPair {
        &self.key_pair
    }
}

/// Testable implementation of a [`Membership`]. Will expose a method to generate a vote token used for testing.
pub trait TestableElection<TYPES: NodeType>: Membership<TYPES> {
    /// Generate a vote token used for testing.
    fn generate_test_vote_token() -> TYPES::VoteTokenType;
}
