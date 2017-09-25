// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;
use std::collections::{BTreeMap, BTreeSet};
use parking_lot::Mutex;
use ethkey::{Public, Secret, Signature};
use key_server_cluster::{Error, NodeId, SessionId, SessionMeta, DocumentKeyShare, KeyStorage};
use key_server_cluster::cluster::Cluster;
use key_server_cluster::cluster_sessions::ClusterSession;
use key_server_cluster::message::{Message, ShareMoveMessage, ShareMoveConsensusMessage,
	ShareMoveRequest, ShareMove, ShareMoveConfirm, ShareMoveError, ConsensusMessageWithServersMap,
	InitializeConsensusSessionWithServersMap, ConfirmConsensusInitialization};
use key_server_cluster::jobs::job_session::JobTransport;
use key_server_cluster::jobs::dummy_job::{DummyJob, DummyJobTransport};
use key_server_cluster::jobs::servers_set_change_access_job::{ServersSetChangeAccessJob, ServersSetChangeAccessRequest};
use key_server_cluster::jobs::consensus_session::{ConsensusSessionParams, ConsensusSessionState, ConsensusSession};

/// Share move session API.
pub trait Session: Send + Sync + 'static {
}

/// Share move session transport.
pub trait SessionTransport: Clone + JobTransport<PartialJobRequest=ServersSetChangeAccessRequest, PartialJobResponse=bool> {
	/// Send message to given node.
	fn send(&self, node: &NodeId, message: ShareMoveMessage) -> Result<(), Error>;
	/// Set share destinations.
	fn set_shares_to_move(&mut self, shares_to_move: BTreeMap<NodeId, NodeId>);
}

/// Share move session.
pub struct SessionImpl<T: SessionTransport> {
	/// Session core.
	core: SessionCore<T>,
	/// Session data.
	data: Mutex<SessionData<T>>,
}

/// Immutable session data.
struct SessionCore<T: SessionTransport> {
	/// Session metadata.
	pub meta: SessionMeta,
	/// Share add session id.
	pub sub_session: Secret,
	/// Session-level nonce.
	pub nonce: u64,
	/// Original key share (for old nodes only). TODO: is it possible to read from key_storage
	pub key_share: Option<DocumentKeyShare>,
	/// Session transport to communicate to other cluster nodes.
	pub transport: T,
	/// Key storage.
	pub key_storage: Arc<KeyStorage>,
}

/// Share move consensus session type.
type ShareMoveChangeConsensusSession<T: SessionTransport> = ConsensusSession<ServersSetChangeAccessJob, T, DummyJob, DummyJobTransport>;

/// Mutable session data.
struct SessionData<T: SessionTransport> {
	/// Session state.
	pub state: SessionState,
	/// Consensus session.
	pub consensus_session: Option<ShareMoveChangeConsensusSession<T>>,
	/// Shares to move.
	pub shares_to_move: Option<BTreeMap<NodeId, NodeId>>,
	/// Move confirmations to receive.
	pub move_confirmations_to_receive: Option<BTreeSet<NodeId>>,
	/// Received key share (filled on destination nodes only).
	pub received_key_share: Option<DocumentKeyShare>,
}

/// SessionImpl creation parameters
pub struct SessionParams<T: SessionTransport> {
	/// Session meta.
	pub meta: SessionMeta,
	/// Sub session identifier.
	pub sub_session: Secret,
	/// Session nonce.
	pub nonce: u64,
	/// Session transport to communicate to other cluster nodes.
	pub transport: T,
	/// Key storage.
	pub key_storage: Arc<KeyStorage>,
}

/// Share move session state.
#[derive(Debug, PartialEq)]
enum SessionState {
	/// State when consensus is establishing.
	ConsensusEstablishing,
	/// Waiting for move confirmation.
	WaitingForMoveConfirmation,
	/// Session is completed.
	Finished,
}

/// Isolated ShareAdd session transport.
#[derive(Clone)]
pub struct IsolatedSessionTransport {
	/// Key id.
	session: SessionId,
	/// Session id.
	sub_session: Secret,
	/// Session-level nonce.
	nonce: u64,
	/// Shares to move between.
	shares_to_move: Option<BTreeMap<NodeId, NodeId>>,
	/// Cluster.
	cluster: Arc<Cluster>,
}

impl<T> SessionImpl<T> where T: SessionTransport {
	/// Create new share addition session.
	pub fn new(params: SessionParams<T>) -> Result<Self, Error> {
		Ok(SessionImpl {
			core: SessionCore {
				meta: params.meta.clone(),
				sub_session: params.sub_session,
				nonce: params.nonce,
				key_share: params.key_storage.get(&params.meta.id).ok(), // ignore error, it will be checked later
				transport: params.transport,
				key_storage: params.key_storage,
			},
			data: Mutex::new(SessionData {
				state: SessionState::ConsensusEstablishing,
				consensus_session: None,
				shares_to_move: None,
				move_confirmations_to_receive: None,
				received_key_share: None,
			}),
		})
	}

	/// Set pre-established consensus data.
	pub fn set_consensus_output(&self, shares_to_move: BTreeMap<NodeId, NodeId>) -> Result<(), Error> {
		let mut data = self.data.lock();

		// check state
		if data.state != SessionState::ConsensusEstablishing || data.consensus_session.is_some() {
			return Err(Error::InvalidStateForRequest);
		}

		let old_id_numbers = self.core.key_share.as_ref().map(|ks| &ks.id_numbers);
		check_shares_to_move(&self.core.meta.self_node_id, &shares_to_move, old_id_numbers)?;

		data.move_confirmations_to_receive = Some(shares_to_move.keys().cloned().collect());
		data.shares_to_move = Some(shares_to_move);

		Ok(())
	}

	/// Initialize share add session on master node.
	pub fn initialize(&self, shares_to_move: BTreeMap<NodeId, NodeId>, old_set_signature: Option<Signature>, new_set_signature: Option<Signature>) -> Result<(), Error> {
		debug_assert_eq!(self.core.meta.self_node_id, self.core.meta.master_node_id);

		let mut data = self.data.lock();

		// check state
		if data.state != SessionState::ConsensusEstablishing || data.consensus_session.is_some() {
			return Err(Error::InvalidStateForRequest);
		}

		// if consensus is not yet established => start consensus session
		let is_consensus_pre_established = data.shares_to_move.is_some();
		if !is_consensus_pre_established {
			let key_share = self.core.key_share.as_ref().ok_or(Error::KeyStorage("key share is not found on master node".into()))?;
			check_shares_to_move(&self.core.meta.self_node_id, &shares_to_move, Some(&key_share.id_numbers))?;

			let old_set_signature = old_set_signature.ok_or(Error::InvalidMessage)?;
			let new_set_signature = new_set_signature.ok_or(Error::InvalidMessage)?;
			let mut all_nodes_set: BTreeSet<_> = key_share.id_numbers.keys().cloned().collect();
			let mut new_nodes_set: BTreeSet<_> = all_nodes_set.clone();
			for (target, source) in &shares_to_move {
				new_nodes_set.remove(source);
				new_nodes_set.insert(target.clone());
				all_nodes_set.insert(target.clone());
			}
			let mut consensus_transport = self.core.transport.clone();
			consensus_transport.set_shares_to_move(shares_to_move.clone());

			let mut consensus_session = ConsensusSession::new(ConsensusSessionParams {
				meta: self.core.meta.clone(),
				consensus_executor: ServersSetChangeAccessJob::new_on_master(Public::default(), // TODO: admin key instead of default
					key_share.id_numbers.keys().cloned().collect(),
					key_share.id_numbers.keys().cloned().collect(),
					new_nodes_set.clone(),
					old_set_signature,
					new_set_signature),
				consensus_transport: consensus_transport,
			})?;
			consensus_session.initialize(all_nodes_set)?;
			data.consensus_session = Some(consensus_session);
			data.move_confirmations_to_receive = Some(shares_to_move.keys().cloned().collect());
			data.shares_to_move = Some(shares_to_move);
			return Ok(());
		}

		// otherwise => start sending ShareAdd-specific messages
		Self::on_consensus_established(&self.core, &mut *data)
	}

	/// Process single message.
	pub fn process_message(&self, sender: &NodeId, message: &ShareMoveMessage) -> Result<(), Error> {
		if self.core.nonce != message.session_nonce() {
			return Err(Error::ReplayProtection);
		}

		match message {
			&ShareMoveMessage::ShareMoveConsensusMessage(ref message) =>
				self.on_consensus_message(sender, message),
			&ShareMoveMessage::ShareMoveRequest(ref message) =>
				self.on_share_move_request(sender, message),
			&ShareMoveMessage::ShareMove(ref message) =>
				self.on_share_move(sender, message),
			&ShareMoveMessage::ShareMoveConfirm(ref message) =>
				self.on_share_move_confirmation(sender, message),
			&ShareMoveMessage::ShareMoveError(ref message) =>
				self.on_session_error(sender, message),
		}
	}

	/// When consensus-related message is received.
	pub fn on_consensus_message(&self, sender: &NodeId, message: &ShareMoveConsensusMessage) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// start slave consensus session if needed
		let mut data = self.data.lock();
		if self.core.meta.self_node_id != self.core.meta.master_node_id {
			if data.consensus_session.is_none() {
				match &message.message {
					&ConsensusMessageWithServersMap::InitializeConsensusSession(ref message) => {
						let current_nodes_set = self.core.key_share.as_ref()
							.map(|ks| ks.id_numbers.keys().cloned().collect())
							.unwrap_or_else(|| message.old_nodes_set.clone().into_iter().map(Into::into).collect());
						data.consensus_session = Some(ConsensusSession::new(ConsensusSessionParams {
							meta: self.core.meta.clone(),
							consensus_executor: ServersSetChangeAccessJob::new_on_slave(Public::default(), // TODO: administrator public
								current_nodes_set,
							),
							consensus_transport: self.core.transport.clone(),
						})?);
					},
					_ => return Err(Error::InvalidStateForRequest),
				}
			}
		}

		let (is_establishing_consensus, is_consensus_established, shares_to_move) = {
			let consensus_session = data.consensus_session.as_mut().ok_or(Error::InvalidMessage)?;
			let is_establishing_consensus = consensus_session.state() == ConsensusSessionState::EstablishingConsensus;
			let shares_to_move = match &message.message {
				&ConsensusMessageWithServersMap::InitializeConsensusSession(ref message) => {
					consensus_session.on_consensus_partial_request(sender, ServersSetChangeAccessRequest::from(message))?;
					Some(message.new_nodes_set.iter()
						.filter(|&(old, new)| old != new)
						.map(|(old, new)| (old.clone().into(), new.clone().into()))
						.collect::<BTreeMap<_, _>>())
				},
				&ConsensusMessageWithServersMap::ConfirmConsensusInitialization(ref message) => {
					consensus_session.on_consensus_partial_response(sender, message.is_confirmed)?;
					None
				},
			};

			(
				is_establishing_consensus,
				consensus_session.state() == ConsensusSessionState::ConsensusEstablished,
				shares_to_move
			)
		};
println!("=== {}: {:?}", self.core.meta.self_node_id, shares_to_move);
		if let Some(shares_to_move) = shares_to_move {
			data.move_confirmations_to_receive = Some(shares_to_move.keys().cloned().collect());
			data.shares_to_move = Some(shares_to_move);
		}
		if self.core.meta.self_node_id != self.core.meta.master_node_id || !is_establishing_consensus || !is_consensus_established {
			return Ok(());
		}

		Self::on_consensus_established(&self.core, &mut *data)
	}

	/*/// When initialization request is received.
	pub fn on_initialize_session(&self, sender: &NodeId, message: &InitializeShareMoveSession) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// awaiting this message from master node only
		if sender != &self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}

		// check shares_to_move
		let shares_to_move = message.shares_to_move.clone().into_iter().map(|(k, v)| (k.into(), v.into())).collect();
		check_shares_to_move(&self.core.meta.self_node_id, &shares_to_move, self.core.key_share.as_ref().map(|ks| &ks.id_numbers))?;

		// this node is either old on both (this && master) nodes, or new on both nodes
		let key_share = if let Some(share_destination) = shares_to_move.get(&self.core.meta.self_node_id) {
			Some(self.core.key_share.as_ref()
				.ok_or(Error::InvalidMessage)?)
		} else {
			if shares_to_move.values().any(|n| n == &self.core.meta.self_node_id) {
				if self.core.key_share.is_some() {
					return Err(Error::InvalidMessage);
				}
			}

			None
		};

		// update state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForInitialization {
			return Err(Error::InvalidStateForRequest);
		}
		data.state = SessionState::WaitingForMoveConfirmation;
		data.shares_to_move.extend(shares_to_move);
		let move_confirmations_to_receive: Vec<_> = data.shares_to_move.values().cloned().collect();
		data.move_confirmations_to_receive.extend(move_confirmations_to_receive);

		// confirm initialization
		self.core.transport.send(sender, ShareMoveMessage::ConfirmShareMoveInitialization(ConfirmShareMoveInitialization {
			session: self.core.meta.id.clone().into(),
			sub_session: self.core.sub_session.clone().into(),
			session_nonce: self.core.nonce,
		}))?;

		Ok(())
	}

	/// When session initialization confirmation message is received.
	pub fn on_confirm_initialization(&self, sender: &NodeId, message: &ConfirmShareMoveInitialization) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// awaiting this message on master node only
		if self.core.meta.self_node_id != self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}

		// check state
		let mut data = self.data.lock();
		if data.state != SessionState::WaitingForInitializationConfirm {
			return Err(Error::InvalidStateForRequest);
		}
		// do not expect double confirmations
		if !data.init_confirmations_to_receive.remove(sender) {
			return Err(Error::InvalidMessage);
		}
		// if not all init confirmations are received => return
		if !data.init_confirmations_to_receive.is_empty() {
			return Ok(());
		}

		// update state
		data.state = SessionState::WaitingForMoveConfirmation;
		// send share move requests
		for share_source in data.shares_to_move.keys().filter(|n| **n != self.core.meta.self_node_id) {
			self.core.transport.send(share_source, ShareMoveMessage::ShareMoveRequest(ShareMoveRequest {
				session: self.core.meta.id.clone().into(),
				sub_session: self.core.sub_session.clone().into(),
				session_nonce: self.core.nonce,
			}))?;
		}
		// move share if required
		if let Some(share_destination) = data.shares_to_move.get(&self.core.meta.self_node_id) {
			Self::move_share(&self.core, share_destination)?;
		}

		Ok(())
	}*/

	/// When share move request is received.
	pub fn on_share_move_request(&self, sender: &NodeId, message: &ShareMoveRequest) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// awaiting this message from master node only
		if sender != &self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}

		// check state
		let mut data = self.data.lock();
		if data.state == SessionState::ConsensusEstablishing {
			data.state = SessionState::WaitingForMoveConfirmation;
		}
		else if data.state != SessionState::WaitingForMoveConfirmation {
			return Err(Error::InvalidStateForRequest);
		}

		// move share
		let shares_to_move = data.shares_to_move.as_ref().expect("TODO");
		if let Some(share_destination) = shares_to_move.iter().filter(|&(_, v)| v == &self.core.meta.self_node_id).map(|(k, _)| k).nth(0) {
			Self::move_share(&self.core, share_destination)
		} else {
			Err(Error::InvalidMessage)
		}
	}

	/// When moving share is received.
	pub fn on_share_move(&self, sender: &NodeId, message: &ShareMove) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// check state
		let mut data = self.data.lock();
		if data.state == SessionState::ConsensusEstablishing {
			data.state = SessionState::WaitingForMoveConfirmation;
		} else if data.state != SessionState::WaitingForMoveConfirmation {
			return Err(Error::InvalidStateForRequest);
		}

		// check that we are expecting this share
		if data.shares_to_move.as_ref().expect("TODO").get(&self.core.meta.self_node_id) != Some(sender) {
			return Err(Error::InvalidMessage);
		}

		// update state
		data.move_confirmations_to_receive.as_mut().expect("TODO").remove(&self.core.meta.self_node_id);
		data.received_key_share = Some(DocumentKeyShare {
			author: message.author.clone().into(),
			threshold: message.threshold,
			id_numbers: message.id_numbers.iter().map(|(k, v)| (k.clone().into(), v.clone().into())).collect(),
			polynom1: message.polynom1.iter().cloned().map(Into::into).collect(),
			secret_share: message.secret_share.clone().into(),
			common_point: message.common_point.clone().map(Into::into),
			encrypted_point: message.encrypted_point.clone().map(Into::into),
		});

		// send confirmation to all other nodes
		{
			let shares_to_move = data.shares_to_move.as_ref().expect("TODO");
			let all_nodes_set: BTreeSet<_> = shares_to_move.keys().cloned()
				.chain(message.id_numbers.keys().cloned().map(Into::into))
				.collect();
			for node in all_nodes_set.into_iter().filter(|n| n != &self.core.meta.self_node_id) {
				self.core.transport.send(&node, ShareMoveMessage::ShareMoveConfirm(ShareMoveConfirm {
					session: self.core.meta.id.clone().into(),
					sub_session: self.core.sub_session.clone().into(),
					session_nonce: self.core.nonce,
				}))?;
			}
		}

		// complete session if this was last share
		if data.move_confirmations_to_receive.as_ref().expect("TODO").is_empty() {
			Self::complete_session(&self.core, &mut *data)?;
		}

		Ok(())
	}

	/// When share is received from destination node.
	pub fn on_share_move_confirmation(&self, sender: &NodeId, message: &ShareMoveConfirm) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.sub_session == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		// check state
		let mut data = self.data.lock();
		if data.state == SessionState::ConsensusEstablishing {
			data.state = SessionState::WaitingForMoveConfirmation;
		} else if data.state != SessionState::WaitingForMoveConfirmation {
			return Err(Error::InvalidStateForRequest);
		}

		// find share source
		{
			let mut move_confirmations_to_receive = data.move_confirmations_to_receive.as_mut().expect("TODO");
			if !move_confirmations_to_receive.remove(sender) {
				return Err(Error::InvalidMessage);
			}
			
			if !move_confirmations_to_receive.is_empty() {
				return Ok(());
			}
		}

		Self::complete_session(&self.core, &mut *data)
	}

	/// When error has occured on another node.
	pub fn on_session_error(&self, sender: &NodeId, message: &ShareMoveError) -> Result<(), Error> {
		let mut data = self.data.lock();

		warn!("{}: share move session failed with error: {} from {}", self.core.meta.self_node_id, message.error, sender);

		data.state = SessionState::Finished;

		Ok(())
	}

	/// Start sending ShareMove-specific messages, when consensus is established.
	fn on_consensus_established(core: &SessionCore<T>, data: &mut SessionData<T>) -> Result<(), Error> {
		// update state
		data.state = SessionState::WaitingForMoveConfirmation;

		// if on master node, send common shared data to every new node
		let is_master_node = core.meta.self_node_id == core.meta.master_node_id;
		if is_master_node {
			Self::disseminate_share_move_requests(core, data)?;
		}
		// move share if required
		let shares_to_move = data.shares_to_move.as_ref().expect("TODO");
		if let Some(share_destination) = shares_to_move.iter().filter(|&(_, v)| v == &core.meta.self_node_id).map(|(k, _)| k).nth(0) {
			Self::move_share(core, share_destination)?;
		}

		// remember move confirmations to receive
		data.move_confirmations_to_receive = Some(shares_to_move.keys().cloned().collect());

		Ok(())
	}

	/// Disseminate share move requests.
	fn disseminate_share_move_requests(core: &SessionCore<T>, data: &mut SessionData<T>) -> Result<(), Error> {
		let shares_to_move = data.shares_to_move.as_ref().expect("TODO");
		for share_source in shares_to_move.values().filter(|n| **n != core.meta.self_node_id) {
			core.transport.send(share_source, ShareMoveMessage::ShareMoveRequest(ShareMoveRequest {
				session: core.meta.id.clone().into(),
				sub_session: core.sub_session.clone().into(),
				session_nonce: core.nonce,
			}))?;
		}

		Ok(())
	}

	/// Send share move message.
	fn move_share(core: &SessionCore<T>, share_destination: &NodeId) -> Result<(), Error> {
		let key_share = core.key_share.as_ref()
			.expect("move_share is called on nodes from shares_to_move.keys(); all 'key' nodes have shares; qed");
		core.transport.send(share_destination, ShareMoveMessage::ShareMove(ShareMove {
			session: core.meta.id.clone().into(),
			sub_session: core.sub_session.clone().into(),
			session_nonce: core.nonce,
			author: key_share.author.clone().into(),
			threshold: key_share.threshold,
			id_numbers: key_share.id_numbers.iter().map(|(k, v)| (k.clone().into(), v.clone().into())).collect(),
			polynom1: key_share.polynom1.iter().cloned().map(Into::into).collect(),
			secret_share: key_share.secret_share.clone().into(),
			common_point: key_share.common_point.clone().map(Into::into),
			encrypted_point: key_share.encrypted_point.clone().map(Into::into),
		}))
	}

	/// Complete session on this node.
	fn complete_session(core: &SessionCore<T>, data: &mut SessionData<T>) -> Result<(), Error> {
		// if we are source node => remove share from storage
		let shares_to_move = data.shares_to_move.as_ref().expect("TODO");
		if shares_to_move.values().any(|n| n == &core.meta.self_node_id) {
			return core.key_storage.remove(&core.meta.id)
				.map_err(|e| Error::KeyStorage(e.into()));
		}

		// else we need to update key_share.id_numbers.keys()
		let is_old_node = data.received_key_share.is_none();
		let mut key_share = data.received_key_share.take()
			.unwrap_or_else(|| core.key_share.as_ref()
				.expect("on target nodes received_key_share is non-empty; on old nodes key_share is not empty; qed")
				.clone());
		for (target_node, source_node) in shares_to_move {
			let id_number = key_share.id_numbers.remove(source_node)
				.expect("source_node is old node; there's entry in id_numbers for each old node; qed");
			key_share.id_numbers.insert(target_node.clone(), id_number);
		}

		// ... and update key share in storage
		if is_old_node {
			core.key_storage.update(core.meta.id.clone(), key_share)
		} else {
			core.key_storage.insert(core.meta.id.clone(), key_share)
		}.map_err(|e| Error::KeyStorage(e.into()))
	}
}

impl<T> ClusterSession for SessionImpl<T> where T: SessionTransport {
	fn is_finished(&self) -> bool {
		self.data.lock().state == SessionState::Finished
	}

	fn on_session_timeout(&self) {
		unimplemented!()
	}

	fn on_node_timeout(&self, _node_id: &NodeId) {
		unimplemented!()
	}
}

impl JobTransport for IsolatedSessionTransport {
	type PartialJobRequest = ServersSetChangeAccessRequest;
	type PartialJobResponse = bool;

	fn send_partial_request(&self, node: &NodeId, request: ServersSetChangeAccessRequest) -> Result<(), Error> {
		let shares_to_move = self.shares_to_move.as_ref().expect("TODO");
		self.cluster.send(node, Message::ShareMove(ShareMoveMessage::ShareMoveConsensusMessage(ShareMoveConsensusMessage {
			session: self.session.clone().into(),
			sub_session: self.sub_session.clone().into(),
			session_nonce: self.nonce,
			message: ConsensusMessageWithServersMap::InitializeConsensusSession(InitializeConsensusSessionWithServersMap {
				old_nodes_set: request.old_servers_set.into_iter().map(Into::into).collect(),
				new_nodes_set: request.new_servers_set.into_iter().map(|n| (n.into(),
					shares_to_move.get(&n).cloned().unwrap_or_else(|| n.clone()).into())).collect(),
				old_set_signature: request.old_set_signature.into(),
				new_set_signature: request.new_set_signature.into(),
			}),
		})))
	}

	fn send_partial_response(&self, node: &NodeId, response: bool) -> Result<(), Error> {
		self.cluster.send(node, Message::ShareMove(ShareMoveMessage::ShareMoveConsensusMessage(ShareMoveConsensusMessage {
			session: self.session.clone().into(),
			sub_session: self.sub_session.clone().into(),
			session_nonce: self.nonce,
			message: ConsensusMessageWithServersMap::ConfirmConsensusInitialization(ConfirmConsensusInitialization {
				is_confirmed: response,
			}),
		})))
	}
}

impl SessionTransport for IsolatedSessionTransport {
	fn set_shares_to_move(&mut self, shares_to_move: BTreeMap<NodeId, NodeId>) {
		self.shares_to_move = Some(shares_to_move);
	}

	fn send(&self, node: &NodeId, message: ShareMoveMessage) -> Result<(), Error> {
		self.cluster.send(node, Message::ShareMove(message))
	}
}

fn check_shares_to_move(self_node_id: &NodeId, shares_to_move: &BTreeMap<NodeId, NodeId>, id_numbers: Option<&BTreeMap<NodeId, Secret>>) -> Result<(), Error> {
	// shares to move must not be empty
	if shares_to_move.is_empty() {
		return Err(Error::InvalidMessage);
	}

	if let Some(id_numbers) = id_numbers {
		// all values in shares_to_move must be old nodes of the session
		if shares_to_move.values().any(|n| !id_numbers.contains_key(n)) {
			return Err(Error::InvalidNodesConfiguration);
		}
		// all keys in shares_to_move must be new nodes for the session
		if shares_to_move.keys().any(|n| id_numbers.contains_key(n)) {
			return Err(Error::InvalidNodesConfiguration);
		}
	} else {
		// this node must NOT in values of shares_to_move
		if shares_to_move.values().any(|n| n == self_node_id) {
			return Err(Error::InvalidMessage);
		}
		// this node must be in keys of share_to_move
		if !shares_to_move.contains_key(self_node_id) {
			return Err(Error::InvalidMessage);
		}
	}

	// all values of the shares_to_move must be distinct
	if shares_to_move.values().collect::<BTreeSet<_>>().len() != shares_to_move.len() {
		return Err(Error::InvalidNodesConfiguration);
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::collections::{VecDeque, BTreeMap, BTreeSet};
	use ethkey::{Random, Generator, Public, KeyPair, sign};
	use key_server_cluster::{NodeId, SessionId, Error, KeyStorage, DummyKeyStorage, SessionMeta};
	use key_server_cluster::cluster::Cluster;
	use key_server_cluster::cluster::tests::DummyCluster;
	use key_server_cluster::generation_session::tests::MessageLoop as GenerationMessageLoop;
	use key_server_cluster::math;
	use key_server_cluster::message::{Message, ServersSetChangeMessage, ShareAddMessage};
	use key_server_cluster::servers_set_change_session::tests::generate_key;
	use key_server_cluster::jobs::servers_set_change_access_job::ordered_nodes_hash;
	use super::{SessionImpl, SessionParams, SessionTransport, IsolatedSessionTransport};

	struct Node {
		pub cluster: Arc<DummyCluster>,
		pub key_storage: Arc<DummyKeyStorage>,
		pub session: SessionImpl<IsolatedSessionTransport>,
	}

	struct MessageLoop {
		pub session_id: SessionId,
		pub nodes: BTreeMap<NodeId, Node>,
		pub queue: VecDeque<(NodeId, NodeId, Message)>,
	}

	impl MessageLoop {
		pub fn new(gml: GenerationMessageLoop, threshold: usize, num_nodes_to_move: usize) -> Self {
			let new_nodes_ids: BTreeSet<_> = (0..num_nodes_to_move).map(|_| Random.generate().unwrap().public().clone()).collect();
			let shares_to_move: BTreeMap<_, _> = gml.nodes.keys().cloned().zip(new_nodes_ids.iter().cloned()).take(num_nodes_to_move).collect();

			let key_id = gml.session_id.clone();
			let session_id = SessionId::default();
			let sub_session = Random.generate().unwrap().secret().clone();
			let mut nodes = BTreeMap::new();
			let master_node_id = gml.nodes.keys().cloned().nth(0).unwrap();
			let meta = SessionMeta {
				self_node_id: master_node_id.clone(),
				master_node_id: master_node_id.clone(),
				id: session_id.clone(),
				threshold: threshold,
			};
 
			for (n, nd) in &gml.nodes {
				let cluster = nd.cluster.clone();
				let key_storage = nd.key_storage.clone();
				let mut meta = meta.clone();
				meta.self_node_id = n.clone();
				let session = SessionImpl::new(SessionParams {
					meta: meta.clone(),
					sub_session: sub_session.clone(),
					transport: IsolatedSessionTransport {
						session: meta.id.clone(),
						sub_session: sub_session.clone(),
						nonce: 1,
						shares_to_move: None,
						cluster: cluster.clone(),
					},
					key_storage: nd.key_storage.clone(),
					nonce: 1,
				}).unwrap();
				nodes.insert(n.clone(), Node {
					cluster: cluster,
					key_storage: key_storage,
					session: session,
				});
			}
			for new_node_id in new_nodes_ids {
				let cluster = Arc::new(DummyCluster::new(new_node_id.clone()));
				let key_storage = Arc::new(DummyKeyStorage::default());
				let mut meta = meta.clone();
				meta.self_node_id = new_node_id;
				let session = SessionImpl::new(SessionParams {
					meta: meta.clone(),
					sub_session: sub_session.clone(),
					transport: IsolatedSessionTransport {
						session: meta.id.clone(),
						sub_session: sub_session.clone(),
						nonce: 1,
						shares_to_move: None,
						cluster: cluster.clone(),
					},
					key_storage: key_storage.clone(),
					nonce: 1,
				}).unwrap();
				nodes.insert(new_node_id, Node {
					cluster: cluster,
					key_storage: key_storage,
					session: session,
				});
			}

			MessageLoop {
				session_id: session_id,
				nodes: nodes,
				queue: Default::default(),
			}
		}

		pub fn run(&mut self) {
			while let Some((from, to, message)) = self.take_message() {
println!("=== {} -> {}: {}", from, to, message);
				self.process_message((from, to, message)).unwrap();
			}
		}

		pub fn take_message(&mut self) -> Option<(NodeId, NodeId, Message)> {
			self.nodes.values()
				.filter_map(|n| n.cluster.take_message().map(|m| (n.session.core.meta.self_node_id.clone(), m.0, m.1)))
				.nth(0)
				.or_else(|| self.queue.pop_front())
		}

		pub fn process_message(&mut self, msg: (NodeId, NodeId, Message)) -> Result<(), Error> {
			match {
				match msg.2 {
					Message::ShareMove(ref message) =>
						self.nodes[&msg.1].session.process_message(&msg.0, message),
					_ => unreachable!("only servers set change messages are expected"),
				}
			} {
				Ok(_) => Ok(()),
				Err(Error::TooEarlyForRequest) => {
					self.queue.push_back(msg);
					Ok(())
				},
				Err(err) => Err(err),
			}
		}
	}

	#[test]
	fn node_moved_using_share_move() {
		// initial 2-of-3 session
		let (t, n) = (1, 3);
		let gml = generate_key(t, n);
		let gml_nodes: BTreeSet<_> = gml.nodes.keys().cloned().collect();
		let key_id = gml.session_id.clone();
		let master = gml.nodes.keys().cloned().nth(0).unwrap();
		let old_nodes_set: BTreeSet<_> = gml.nodes.keys().cloned().collect();
		let source_node = gml.nodes.keys().cloned().nth(1).unwrap();
		let joint_secret = math::compute_joint_secret(gml.nodes.values()
			.map(|nd| nd.key_storage.get(&key_id).unwrap().polynom1[0].clone())
			.collect::<Vec<_>>()
			.iter()).unwrap();
		let joint_key_pair = KeyPair::from_secret(joint_secret.clone()).unwrap();

		// add 1 node && move share
		let mut ml = MessageLoop::new(gml, 3, 1);
		let new_nodes_set: BTreeSet<_> = ml.nodes.keys().cloned().filter(|n| !gml_nodes.contains(n)).collect();
		let old_set_signature = sign(&joint_secret, &ordered_nodes_hash(&old_nodes_set)).unwrap();
		let new_set_signature = sign(&joint_secret, &ordered_nodes_hash(&new_nodes_set)).unwrap();
		let target_node = new_nodes_set.into_iter().nth(0).unwrap();
println!("=== moving from {} to {}", source_node, target_node);
		let shares_to_move = vec![(target_node, source_node.clone())].into_iter().collect();
		ml.nodes[&master].session.initialize(shares_to_move, Some(old_set_signature), Some(new_set_signature)).unwrap();
		ml.run();

		// try to recover secret for every possible combination of nodes && check that secret is the same
		let document_secret_plain = math::generate_random_point().unwrap();
		for n1 in 0..n+1 {
			for n2 in n1+1..n+1 {
				let node1 = ml.nodes.keys().nth(n1).unwrap();
				let node2 = ml.nodes.keys().nth(n2).unwrap();
				if node1 == &source_node {
					assert!(ml.nodes.values().nth(n1).unwrap().key_storage.get(&key_id).is_err());
					continue;
				}
				if node2 == &source_node {
					assert!(ml.nodes.values().nth(n2).unwrap().key_storage.get(&key_id).is_err());
					continue;
				}

				let share1 = ml.nodes.values().nth(n1).unwrap().key_storage.get(&key_id).unwrap();
				let share2 = ml.nodes.values().nth(n2).unwrap().key_storage.get(&key_id).unwrap();
				let id_number1 = share1.id_numbers[ml.nodes.keys().nth(n1).unwrap()].clone();
				let id_number2 = share1.id_numbers[ml.nodes.keys().nth(n2).unwrap()].clone();

				// now encrypt and decrypt data
				let (document_secret_decrypted, document_secret_decrypted_test) =
					math::tests::do_encryption_and_decryption(t,
						joint_key_pair.public(),
						&[id_number1, id_number2],
						&[share1.secret_share, share2.secret_share],
						Some(&joint_secret),
						document_secret_plain.clone());

				assert_eq!(document_secret_plain, document_secret_decrypted_test);
				assert_eq!(document_secret_plain, document_secret_decrypted);
			}
		}
	}
}
