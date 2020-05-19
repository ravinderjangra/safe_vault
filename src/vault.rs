// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{
    action::{Action, ConsensusAction},
    client_handler::ClientHandler,
    data_handler::DataHandler,
    routing::{
        event::Event as RoutingEvent, DstLocation, Node, SrcLocation, TransportEvent as ClientEvent,
    },
    rpc::Rpc,
    utils, Config, Result,
};
use crossbeam_channel::{Receiver, Select};
use log::{debug, error, info, trace, warn};
use rand::{CryptoRng, Rng, SeedableRng};
use rand_chacha::ChaChaRng;
use safe_nd::{ClientRequest, CoinsRequest, LoginPacketRequest, NodeFullId, Request, XorName};
use std::borrow::Cow;
use std::{
    cell::{Cell, RefCell},
    fmt::{self, Display, Formatter},
    fs,
    net::SocketAddr,
    path::PathBuf,
    rc::Rc,
};

const STATE_FILENAME: &str = "state";

#[allow(clippy::large_enum_variant)]
enum State {
    Elder {
        client_handler: ClientHandler,
        data_handler: DataHandler,
    },
    Adult {
        data_handler: DataHandler,
    },
}

/// Specifies whether to try loading cached data from disk, or to just construct a new instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Load,
    New,
}

/// Command that the user can send to a running vault to control its execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Shutdown the vault
    Shutdown,
}

/// Main vault struct.
pub struct Vault<R: CryptoRng + Rng> {
    id: NodeFullId,
    root_dir: PathBuf,
    state: State,
    event_receiver: Receiver<RoutingEvent>,
    client_receiver: Receiver<ClientEvent>,
    command_receiver: Receiver<Command>,
    routing_node: Rc<RefCell<Node>>,
    rng: R,
}

impl<R: CryptoRng + Rng> Vault<R> {
    /// Create and start vault. This will block until a `Command` to free it is fired.
    pub fn new(
        routing_node: Node,
        event_receiver: Receiver<RoutingEvent>,
        client_receiver: Receiver<ClientEvent>,
        config: &Config,
        command_receiver: Receiver<Command>,
        mut rng: R,
    ) -> Result<Self> {
        let mut init_mode = Init::Load;

        let (is_elder, id) = Self::read_state(&config)?.unwrap_or_else(|| {
            let id = NodeFullId::new(&mut rng);
            init_mode = Init::New;
            (false, id)
        });

        #[cfg(feature = "mock_parsec")]
        {
            trace!(
                "creating vault {:?} with routing_id {:?}",
                id.public_id().name(),
                routing_node.id()
            );
        }

        let root_dir = config.root_dir()?;
        let root_dir = root_dir.as_path();

        let routing_node = Rc::new(RefCell::new(routing_node));

        let state = if is_elder {
            let total_used_space = Rc::new(Cell::new(0));
            let client_handler = ClientHandler::new(
                id.public_id().clone(),
                &config,
                &total_used_space,
                init_mode,
                routing_node.clone(),
            )?;
            let data_handler = DataHandler::new(
                id.public_id().clone(),
                &config,
                &total_used_space,
                init_mode,
                is_elder,
                routing_node.clone(),
            )?;
            State::Elder {
                client_handler,
                data_handler,
            }
        } else {
            info!("Initializing new Vault as Adult");
            let total_used_space = Rc::new(Cell::new(0));
            let data_handler = DataHandler::new(
                id.public_id().clone(),
                &config,
                &total_used_space,
                init_mode,
                false,
                routing_node.clone(),
            )?;
            State::Adult { data_handler }
        };

        let vault = Self {
            id,
            root_dir: root_dir.to_path_buf(),
            state,
            event_receiver,
            client_receiver,
            command_receiver,
            routing_node,
            rng,
        };
        vault.dump_state()?;
        Ok(vault)
    }

    /// Returns our connection info.
    pub fn our_connection_info(&mut self) -> Result<SocketAddr> {
        self.routing_node
            .borrow_mut()
            .our_connection_info()
            .map_err(From::from)
    }

    #[cfg(any(feature = "mock_parsec", feature = "mock"))]
    /// Returns whether routing node is in elder state.
    pub fn is_elder(&mut self) -> bool {
        self.routing_node.borrow().is_elder()
    }

    /// Runs the main event loop. Blocks until the vault is terminated.
    // FIXME: remove when https://github.com/crossbeam-rs/crossbeam/issues/404 is resolved
    #[allow(clippy::zero_ptr, clippy::drop_copy)]
    pub fn run(&mut self) {
        loop {
            let mut sel = Select::new();

            let mut r_node = self.routing_node.borrow_mut();
            r_node.register(&mut sel);
            let routing_event_rx_idx = sel.recv(&self.event_receiver);
            let client_network_rx_idx = sel.recv(&self.client_receiver);
            let command_rx_idx = sel.recv(&self.command_receiver);

            let selected_operation = sel.ready();
            drop(r_node);

            match selected_operation {
                idx if idx == client_network_rx_idx => {
                    let event = match self.client_receiver.recv() {
                        Ok(ev) => ev,
                        Err(e) => panic!("FIXME: {:?}", e),
                    };
                    self.step_client(event);
                }
                idx if idx == routing_event_rx_idx => {
                    let event = match self.event_receiver.recv() {
                        Ok(ev) => ev,
                        Err(e) => panic!("FIXME: {:?}", e),
                    };
                    self.step_routing(event);
                }
                idx if idx == command_rx_idx => {
                    let command = match self.command_receiver.recv() {
                        Ok(ev) => ev,
                        Err(e) => panic!("FIXME: {:?}", e),
                    };
                    match command {
                        Command::Shutdown => break,
                    }
                }
                idx => {
                    if let Err(err) = self
                        .routing_node
                        .borrow_mut()
                        .handle_selected_operation(idx)
                    {
                        warn!("Could not process operation: {}", err);
                    }
                }
            }
        }
    }

    fn promote_to_elder(&mut self) -> Result<()> {
        let mut config = Config::default();
        config.set_root_dir(self.root_dir.clone());
        let total_used_space = Rc::new(Cell::new(0));
        let client_handler = ClientHandler::new(
            self.id.public_id().clone(),
            &config,
            &total_used_space,
            Init::New,
            self.routing_node.clone(),
        )?;
        let data_handler = DataHandler::new(
            self.id.public_id().clone(),
            &config,
            &total_used_space,
            Init::New,
            true,
            self.routing_node.clone(),
        )?;
        self.state = State::Elder {
            client_handler,
            data_handler,
        };
        Ok(())
    }

    /// Processes any outstanding network events and returns. Does not block.
    /// Returns whether at least one event was processed.
    pub fn poll(&mut self) -> bool {
        let mut _processed = false;
        loop {
            let mut sel = Select::new();
            let mut r_node = self.routing_node.borrow_mut();
            r_node.register(&mut sel);
            let routing_event_rx_idx = sel.recv(&self.event_receiver);
            let client_network_rx_idx = sel.recv(&self.client_receiver);
            let command_rx_idx = sel.recv(&self.command_receiver);

            if let Ok(selected_operation) = sel.try_ready() {
                drop(r_node);

                match selected_operation {
                    idx if idx == client_network_rx_idx => {
                        let event = match self.client_receiver.recv() {
                            Ok(ev) => ev,
                            Err(e) => panic!("FIXME: {:?}", e),
                        };
                        self.step_client(event);
                        _processed = true;
                    }
                    idx if idx == routing_event_rx_idx => {
                        let event = match self.event_receiver.recv() {
                            Ok(ev) => ev,
                            Err(e) => panic!("FIXME: {:?}", e),
                        };
                        self.step_routing(event);
                        _processed = true;
                    }
                    idx if idx == command_rx_idx => {
                        let command = match self.command_receiver.recv() {
                            Ok(ev) => ev,
                            Err(e) => panic!("FIXME: {:?}", e),
                        };
                        match command {
                            Command::Shutdown => (),
                        }
                        _processed = true;
                    }
                    idx => {
                        if let Err(err) = self
                            .routing_node
                            .borrow_mut()
                            .handle_selected_operation(idx)
                        {
                            warn!("Could not process operation: {}", err);
                            break;
                        }
                    }
                }
            } else {
                break;
            }
        }

        _processed
    }

    fn step_routing(&mut self, event: RoutingEvent) {
        debug!("Received routing event: {:?}", event);
        let mut maybe_action = self.handle_routing_event(event);
        while let Some(action) = maybe_action {
            maybe_action = self.handle_action(action);
        }
    }

    fn step_client(&mut self, event: ClientEvent) {
        let mut maybe_action = self.handle_client_event(event);
        while let Some(action) = maybe_action {
            maybe_action = self.handle_action(action);
        }
    }

    fn handle_routing_event(&mut self, event: RoutingEvent) -> Option<Action> {
        match event {
            RoutingEvent::Consensus(custom_event) => {
                match bincode::deserialize::<ConsensusAction>(&custom_event) {
                    Ok(consensus_action) => {
                        let client_handler = self.client_handler_mut()?;
                        client_handler.handle_consensused_action(consensus_action)
                    }
                    Err(e) => {
                        error!("Invalid ConsensusAction passed from Routing: {:?}", e);
                        None
                    }
                }
            }
            RoutingEvent::Promoted => self.promote_to_elder().map_or_else(
                |err| {
                    error!("Error when promoting Vault to Elder: {:?}", err);
                    None
                },
                |()| {
                    info!("Vault promoted to Elder");
                    None
                },
            ),
            RoutingEvent::MessageReceived { content, src, dst } => {
                info!(
                    "Received message: {:?}\n Sent from {:?} to {:?}",
                    content, src, dst
                );
                self.handle_routing_message(src, content)
            }
            RoutingEvent::MemberLeft { name, age: _u8 } => {
                info!("A node has left the section. Node: {:?}", name);
                let get_copy_actions = self.data_handler_mut()?
                .handle_node_left_action(XorName(name.0));
                if let Some(copy_actions) = get_copy_actions {
                    for action in copy_actions {
                        let _ = self.handle_action(action);
                    }
                };
                None
            }
            // Ignore all other events
            _ => None,
        }
    }

    fn handle_routing_message(&mut self, src: SrcLocation, message: Vec<u8>) -> Option<Action> {
        match bincode::deserialize::<Rpc>(&message) {
            Ok(rpc) => match rpc {
                Rpc::Request {
                    request: Request::LoginPacket(LoginPacketRequest::Create(_)),
                    ..
                }
                | Rpc::Request {
                    request: Request::LoginPacket(LoginPacketRequest::CreateFor { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::Coins(CoinsRequest::CreateBalance { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::Coins(CoinsRequest::Transfer { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::LoginPacket(LoginPacketRequest::Update(..)),
                    ..
                }
                | Rpc::Request {
                    request: Request::Client(ClientRequest::InsAuthKey { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::Client(ClientRequest::DelAuthKey { .. }),
                    ..
                } => self
                    .client_handler_mut()?
                    .handle_vault_rpc(utils::get_source_name(src), rpc),
                _ => self
                    .data_handler_mut()?
                    .handle_vault_rpc(utils::get_source_name(src), rpc),
            },
            Err(e) => {
                error!("Error deserializing routing message into Rpc type: {:?}", e);
                None
            }
        }
    }

    fn handle_client_event(&mut self, event: ClientEvent) -> Option<Action> {
        use ClientEvent::*;

        let mut rng = ChaChaRng::from_seed(self.rng.gen());

        let client_handler = self.client_handler_mut()?;
        match event {
            ConnectedTo { peer } => client_handler.handle_new_connection(peer.peer_addr()),
            ConnectionFailure { peer, .. } => {
                client_handler.handle_connection_failure(peer.peer_addr());
            }
            NewMessage { peer, msg } => {
                return client_handler.handle_client_message(peer.peer_addr(), &msg, &mut rng);
            }
            SentUserMessage { peer, .. } => {
                trace!(
                    "{}: Succesfully sent message to: {}",
                    self,
                    peer.peer_addr()
                );
            }
            UnsentUserMessage { peer, .. } => {
                info!("{}: Not sent message to: {}", self, peer.peer_addr());
            }
            BootstrapFailure | BootstrappedTo { .. } => {
                error!("unexpected bootstrapping client event")
            }
            Finish => {
                info!("{}: Received Finish event", self);
            }
        }
        None
    }

    #[allow(dead_code)]
    fn vote_for_action(&mut self, action: &ConsensusAction) -> Option<Action> {
        self.routing_node
            .borrow_mut()
            .vote_for_user_event(utils::serialise(&action))
            .map_or_else(
                |_err| {
                    error!("Cannot vote. Vault is not an elder");
                    None
                },
                |()| None,
            )
    }

    fn handle_action(&mut self, action: Action) -> Option<Action> {
        trace!("{} handle action {:?}", self, action);
        use Action::*;
        match action {
            // Bypass client requests
            VoteFor(action) => self.vote_for_action(&action),
            // VoteFor(action) => self.client_handler_mut()?.handle_consensused_action(action),
            ForwardClientRequest(rpc) => self.forward_client_request(rpc),
            ProxyClientRequest(rpc) => self.proxy_client_request(rpc),
            RespondToOurDataHandlers { sender, rpc } => {
                // TODO - once Routing is integrated, we'll construct the full message to send
                //        onwards, and then if we're also part of the data handlers, we'll call that
                //        same handler which Routing will call after receiving a message.

                self.respond_to_data_handlers(sender, rpc)
            }
            RespondToClientHandlers { sender, rpc } => {
                let client_name = utils::requester_address(&rpc);

                // TODO - once Routing is integrated, we'll construct the full message to send
                //        onwards, and then if we're also part of the client handlers, we'll call that
                //        same handler which Routing will call after receiving a message.

                if self.self_is_handler_for(client_name) {
                    return self.client_handler_mut()?.handle_vault_rpc(sender, rpc);
                }
                None
            }
            SendToPeers {
                sender,
                targets,
                rpc,
            } => {
                let mut next_action = None;
                for target in targets {
                    if target == *self.id.public_id().name() {
                        next_action = self
                            .data_handler_mut()?
                            .handle_vault_rpc(sender, rpc.clone());
                    } else {
                        next_action = self.send_message_to_peer(target, rpc.clone());
                    }
                }
                next_action
            }
            RespondToClient {
                message_id,
                response,
            } => {
                self.client_handler_mut()?
                    .respond_to_client(message_id, response);
                None
            }
        }
    }

    fn respond_to_data_handlers(&self, target: XorName, rpc: Rpc) -> Option<Action> {
        let name = *self.routing_node.borrow().id().name();
        self.routing_node
            .borrow_mut()
            .send_message(
                SrcLocation::Node(name),
                DstLocation::Node(routing::XorName(target.0)),
                utils::serialise(&rpc),
            )
            .map_or_else(
                |err| {
                    error!("Unable to respond to data handler: {:?}", err);
                    None
                },
                |()| {
                    info!("Responded to data handler at {:?} with: {:?}", target, &rpc);
                    None
                },
            )
    }

    fn send_message_to_peer(&self, target: XorName, rpc: Rpc) -> Option<Action> {
        let id = *self.routing_node.borrow().id();
        self.routing_node
            .borrow_mut()
            .send_message(
                SrcLocation::Node(*id.name()),
                DstLocation::Node(routing::XorName(target.0)),
                utils::serialise(&rpc),
            )
            .map_or_else(
                |err| {
                    error!("Unable to send message to Peer: {:?}", err);
                    None
                },
                |()| {
                    info!("Sent message to Peer: {:?}", target);
                    None
                },
            )
    }

    fn forward_client_request(&mut self, rpc: Rpc) -> Option<Action> {
        trace!("{} received a client request {:?}", self, rpc);
        let requester_name = if let Rpc::Request {
            request: Request::LoginPacket(LoginPacketRequest::CreateFor { ref new_owner, .. }),
            ..
        } = rpc
        {
            XorName::from(*new_owner)
        } else {
            *utils::requester_address(&rpc)
        };
        let dst_address = if let Rpc::Request { ref request, .. } = rpc {
            match request.dest_address() {
                Some(address) => address,
                None => {
                    if let Request::Client(ClientRequest::InsAuthKey { .. })
                    | Request::Client(ClientRequest::DelAuthKey { .. }) = request
                    {
                        Cow::Borrowed(self.id.public_id().name())
                    } else {
                        error!("{}: Logic error - no data handler address available.", self);
                        return None;
                    }
                }
            }
        } else {
            error!("{}: Logic error - unexpected RPC.", self);
            return None;
        };

        // TODO - once Routing is integrated, we'll construct the full message to send
        //        onwards, and then if we're also part of the data handlers, we'll call that
        //        same handler which Routing will call after receiving a message.

        if self.self_is_handler_for(&dst_address) {
            // TODO - We need a better way for determining which handler should be given the
            //        message.
            return match rpc {
                Rpc::Request {
                    request: Request::LoginPacket(LoginPacketRequest::Create(_)),
                    ..
                }
                | Rpc::Request {
                    request: Request::LoginPacket(LoginPacketRequest::CreateFor { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::Coins(CoinsRequest::CreateBalance { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::Coins(CoinsRequest::Transfer { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::LoginPacket(LoginPacketRequest::Update(..)),
                    ..
                }
                | Rpc::Request {
                    request: Request::Client(ClientRequest::InsAuthKey { .. }),
                    ..
                }
                | Rpc::Request {
                    request: Request::Client(ClientRequest::DelAuthKey { .. }),
                    ..
                } => self
                    .client_handler_mut()?
                    .handle_vault_rpc(requester_name, rpc),
                _ => self
                    .data_handler_mut()?
                    .handle_vault_rpc(requester_name, rpc),
            };
        }
        None
    }

    fn proxy_client_request(&mut self, rpc: Rpc) -> Option<Action> {
        let requester_name = *utils::requester_address(&rpc);
        let dst_address = if let Rpc::Request {
            request: Request::LoginPacket(LoginPacketRequest::CreateFor { ref new_owner, .. }),
            ..
        } = rpc
        {
            XorName::from(*new_owner)
        } else {
            error!("{}: Logic error - unexpected RPC.", self);
            return None;
        };

        // TODO - once Routing is integrated, we'll construct the full message to send
        //        onwards, and then if we're also part of the data handlers, we'll call that
        //        same handler which Routing will call after receiving a message.

        if self.self_is_handler_for(&dst_address) {
            return self
                .client_handler_mut()?
                .handle_vault_rpc(requester_name, rpc);
        }
        None
    }

    fn self_is_handler_for(&self, _address: &XorName) -> bool {
        true
    }

    // TODO - remove this
    #[allow(unused)]
    fn client_handler(&self) -> Option<&ClientHandler> {
        match &self.state {
            State::Elder {
                ref client_handler, ..
            } => Some(client_handler),
            State::Adult { .. } => None,
        }
    }

    fn client_handler_mut(&mut self) -> Option<&mut ClientHandler> {
        match &mut self.state {
            State::Elder {
                ref mut client_handler,
                ..
            } => Some(client_handler),
            State::Adult { .. } => None,
        }
    }

    // TODO - remove this
    #[allow(unused)]
    fn data_handler(&self) -> Option<&DataHandler> {
        match &self.state {
            State::Elder {
                ref data_handler, ..
            } => Some(data_handler),
            State::Adult { ref data_handler } => Some(data_handler),
        }
    }

    fn data_handler_mut(&mut self) -> Option<&mut DataHandler> {
        match &mut self.state {
            State::Elder {
                ref mut data_handler,
                ..
            } => Some(data_handler),
            State::Adult { .. } => None,
        }
    }

    fn dump_state(&self) -> Result<()> {
        let path = self.root_dir.join(STATE_FILENAME);
        let is_elder = matches!(self.state, State::Elder { .. });
        Ok(fs::write(path, utils::serialise(&(is_elder, &self.id)))?)
    }

    /// Returns Some((is_elder, ID)) or None if file doesn't exist.
    fn read_state(config: &Config) -> Result<Option<(bool, NodeFullId)>> {
        let path = config.root_dir()?.join(STATE_FILENAME);
        if !path.is_file() {
            return Ok(None);
        }
        let contents = fs::read(path)?;
        Ok(Some(bincode::deserialize(&contents)?))
    }
}

impl<R: CryptoRng + Rng> Display for Vault<R> {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "{}", self.id.public_id())
    }
}
