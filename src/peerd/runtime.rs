// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020-2022 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use std::collections::HashSet;
use std::sync::Arc;
use std::thread::spawn;
use std::time::{Duration, SystemTime};

use amplify::Bipolar;
use bitcoin::secp256k1::rand::{self, Rng, RngCore};
use bitcoin::secp256k1::PublicKey;
use internet2::addr::InetSocketAddr;
use internet2::{presentation, transport, zmqsocket, CreateUnmarshaller, ZmqType, ZMQ_CONTEXT};
use lnp::p2p::{bifrost, bolt};
use lnp_rpc::{ClientId, RpcMsg};
use microservices::esb::{self, Handler};
use microservices::node::TryService;
use microservices::peer::supervisor::RuntimeParams;
use microservices::peer::{self, PeerConnection, PeerSender, SendMessage};

use crate::bus::{BusMsg, CtlMsg, ServiceBus};
use crate::rpc::{PeerInfo, ServiceId};
use crate::{Config, Endpoints, Error, LogStyle, Responder, Service};

pub fn run(
    connection: PeerConnection,
    params: RuntimeParams<Config<super::Config>>,
) -> Result<(), Error> {
    debug!("Splitting connection into receiver and sender parts");
    let (receiver, sender) = connection.split();

    debug!("Opening bridge between runtime and peer listener threads");
    let tx = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    let rx = ZMQ_CONTEXT.socket(zmq::PAIR)?;
    tx.connect("inproc://bridge")?;
    rx.bind("inproc://bridge")?;

    let identity = ServiceId::Peer(params.id);

    debug!("Starting thread listening for messages from the remote peer");
    let bridge_handler = ListenerRuntime {
        identity: identity.clone(),
        bridge: esb::Controller::with(
            map! {
                ServiceBus::Bridge => esb::BusConfig {
                    carrier: zmqsocket::Carrier::Socket(tx),
                    router: None,
                    queued: true,
                }
            },
            BridgeHandler,
            ZmqType::Rep,
        )?,
    };
    let listener =
        peer::Listener::with(receiver, bridge_handler, bolt::Messages::create_unmarshaller());
    spawn(move || listener.run_or_panic("peerd-listener"));
    // TODO: Use the handle returned by spawn to track the child process

    debug!("Staring main service runtime");
    let runtime = Runtime {
        identity,
        local_id: params.local_id,
        remote_id: params.remote_id,
        local_socket: params.local_socket,
        remote_socket: params.remote_socket,
        channels: empty!(),
        sender,
        connect: params.connect,
        started: SystemTime::now(),
        messages_sent: 0,
        messages_received: 0,
        awaited_pong: None,
    };
    let config = Config::with(params.config, super::Config {});
    let mut service = Service::service(config, runtime)?;
    service.add_loopback(rx)?;
    service.run_loop()?;
    unreachable!()
}

pub struct BridgeHandler;

impl esb::Handler<ServiceBus> for BridgeHandler {
    type Request = BusMsg;
    type Error = Error;

    fn identity(&self) -> ServiceId { ServiceId::Loopback }

    fn handle(
        &mut self,
        _: &mut Endpoints,
        _: ServiceBus,
        _: ServiceId,
        _: BusMsg,
    ) -> Result<(), Error> {
        // Bridge does not receive replies for now
        Ok(())
    }

    fn handle_err(
        &mut self,
        _: &mut Endpoints,
        err: esb::Error<ServiceId>,
    ) -> Result<(), Self::Error> {
        // We simply propagate the error since it's already being reported
        Err(err.into())
    }
}

pub struct ListenerRuntime {
    identity: ServiceId,
    bridge: esb::Controller<ServiceBus, BusMsg, BridgeHandler>,
}

impl ListenerRuntime {
    fn send_over_bridge(&mut self, req: BusMsg) -> Result<(), Error> {
        debug!("Forwarding LN P2P message over BRIDGE interface to the runtime");
        self.bridge.send_to(ServiceBus::Bridge, self.identity.clone(), req)?;
        Ok(())
    }
}

impl peer::Handler<bolt::Messages> for ListenerRuntime {
    type Error = crate::Error;

    fn handle(&mut self, message: Arc<bolt::Messages>) -> Result<(), Self::Error> {
        // Forwarding all received messages to the runtime
        debug!("New message from remote peer: {}", message);
        trace!("{:#?}", message);
        self.send_over_bridge(BusMsg::Bolt((*message).clone()))
    }

    fn handle_err(&mut self, err: Self::Error) -> Result<(), Self::Error> {
        match err {
            Error::Peer(presentation::Error::Transport(transport::Error::TimedOut)) => {
                trace!("Time to ping the remote peer");
                // This means socket reading timeout and the fact that we need
                // to send a ping message
                self.send_over_bridge(BusMsg::Ctl(CtlMsg::PingPeer))
            }
            // for all other error types, indicating internal errors, we
            // propagate error to the upper level
            _ => {
                error!("Unrecoverable {}, halting", err);
                Err(err)
            }
        }
    }
}

pub struct Runtime {
    identity: ServiceId,
    local_id: PublicKey,
    remote_id: Option<PublicKey>,
    local_socket: Option<InetSocketAddr>,
    remote_socket: InetSocketAddr,

    sender: PeerSender,
    connect: bool,

    channels: HashSet<bolt::ActiveChannelId>,
    started: SystemTime,
    messages_sent: usize,
    messages_received: usize,
    awaited_pong: Option<u16>,
}

impl Responder for Runtime {}

// TODO: Move most of these methods into `Responder` trait
impl esb::Handler<ServiceBus> for Runtime {
    type Request = BusMsg;
    type Error = Error;

    fn identity(&self) -> ServiceId { self.identity.clone() }

    fn on_ready(&mut self, _: &mut Endpoints) -> Result<(), Error> {
        if self.connect {
            info!("{} with the remote peer", "Initializing connection".promo());

            self.sender.send_message(bolt::Messages::Init(bolt::Init {
                global_features: none!(),
                local_features: none!(),
                assets: none!(),
                unknown_tlvs: none!(),
            }))?;

            self.connect = false;
        }
        Ok(())
    }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        message: BusMsg,
    ) -> Result<(), Self::Error> {
        match (bus, message, source) {
            #[cfg(feature = "bolt")]
            (ServiceBus::Msg, BusMsg::Bolt(msg), source) => {
                self.handle_bolt(endpoints, source, msg)
            }
            #[cfg(feature = "bifrost")]
            (ServiceBus::Msg, BusMsg::Birfost(msg), source) => {
                self.handle_bifrost(endpoints, source, msg)
            }
            (ServiceBus::Ctl, BusMsg::Ctl(msg), source) => self.handle_ctl(endpoints, source, msg),
            (ServiceBus::Bridge, msg, _) => self.handle_bridge(endpoints, msg),
            (ServiceBus::Rpc, BusMsg::Rpc(msg), ServiceId::Client(client_id)) => {
                self.handle_rpc(endpoints, client_id, msg)
            }
            (ServiceBus::Rpc, BusMsg::Rpc(_), service) => {
                unreachable!("lnpd received RPC message not from a client but from {}", service)
            }
            (bus, msg, _) => Err(Error::wrong_esb_msg(bus, &msg)),
        }
    }

    fn handle_err(
        &mut self,
        _: &mut Endpoints,
        _: esb::Error<ServiceId>,
    ) -> Result<(), Self::Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    #[cfg(feature = "bolt")]
    fn handle_bolt(
        &mut self,
        _: &mut Endpoints,
        _source: ServiceId,
        message: bolt::Messages,
    ) -> Result<(), Error> {
        debug!("Sending remote peer {}", message);
        trace!("{:#?}", message);
        self.messages_sent += 1;
        self.sender.send_message(message.clone())?;

        match message {
            bolt::Messages::OpenChannel(open_channel) => {
                self.channels
                    .insert(bolt::ActiveChannelId::Temporary(open_channel.temporary_channel_id));
            }
            bolt::Messages::AcceptChannel(accept_channel) => {
                self.channels
                    .insert(bolt::ActiveChannelId::Temporary(accept_channel.temporary_channel_id));
            }
            bolt::Messages::FundingCreated(funding_created) => {
                self.channels.remove(&bolt::ActiveChannelId::Temporary(
                    funding_created.temporary_channel_id,
                ));
                self.channels.insert(bolt::ActiveChannelId::Static(bolt::ChannelId::with(
                    funding_created.funding_txid,
                    funding_created.funding_output_index,
                )));
            }
            bolt::Messages::FundingSigned(_) => {
                // We ingore this message since we rename the channel upon receiving of
                // `FundingCreated`
            }
            bolt::Messages::ChannelReestablish(channel_reestablish) => {
                self.channels.insert(bolt::ActiveChannelId::Static(channel_reestablish.channel_id));
            }
            _ => {} // Do nothing here
        }

        Ok(())
    }

    #[cfg(feature = "bifrost")]
    fn handle_bifrost(
        &mut self,
        _: &mut Endpoints,
        _source: ServiceId,
        message: bifrost::Messages,
    ) -> Result<(), Error> {
        debug!("Sending remote peer {}", message);
        trace!("{:#?}", message);
        self.messages_sent += 1;
        self.sender.send_message(message.clone())?;

        match message {
            _ => {} // Do nothing here
        }

        Ok(())
    }

    fn handle_ctl(
        &mut self,
        _endpoints: &mut Endpoints,
        _source: ServiceId,
        request: CtlMsg,
    ) -> Result<(), Error> {
        #[allow(clippy::match_single_binding)]
        match request {
            _ => {
                error!("Request is not supported by the CTL interface");
                Err(Error::wrong_esb_msg(ServiceBus::Ctl, &request))
            }
        }
    }

    fn handle_bridge(&mut self, endpoints: &mut Endpoints, request: BusMsg) -> Result<(), Error> {
        debug!("BRIDGE RPC request: {}", request);

        if let BusMsg::Bolt(_) = request {
            self.messages_received += 1;
        }

        match &request {
            BusMsg::Ctl(CtlMsg::PingPeer) => {
                self.ping()?;
            }

            BusMsg::Bolt(bolt::Messages::Ping(bolt::Ping { pong_size, .. })) => {
                self.pong(*pong_size)?;
            }

            BusMsg::Bolt(bolt::Messages::Pong(noise)) => {
                match self.awaited_pong {
                    None => warn!("Unexpected pong from the remote peer"),
                    Some(len) if len as usize != noise.len() => {
                        warn!("Pong data size does not match requested with ping")
                    }
                    _ => trace!("Got pong reply, exiting pong await mode"),
                }
                self.awaited_pong = None;
            }

            BusMsg::Bolt(bolt::Messages::ChannelReestablish(_))
            | BusMsg::Bolt(bolt::Messages::OpenChannel(_)) => {
                endpoints.send_to(
                    ServiceBus::Msg,
                    self.identity(),
                    ServiceId::LnpBroker,
                    request,
                )?;
            }

            BusMsg::Bolt(bolt::Messages::AcceptChannel(accept_channel)) => {
                let channeld: ServiceId = accept_channel.temporary_channel_id.into();
                endpoints.send_to(ServiceBus::Msg, self.identity(), channeld, request)?;
            }

            BusMsg::Bolt(bolt::Messages::FundingCreated(bolt::FundingCreated {
                temporary_channel_id,
                funding_txid,
                funding_output_index,
                ..
            })) => {
                let temp_channel_id = bolt::ActiveChannelId::Temporary(*temporary_channel_id);
                let channel_id = bolt::ActiveChannelId::Static(bolt::ChannelId::with(
                    *funding_txid,
                    *funding_output_index,
                ));
                endpoints.send_to(
                    ServiceBus::Msg,
                    self.identity(),
                    (*temporary_channel_id).into(),
                    request,
                )?;
                self.channels.remove(&temp_channel_id);
                self.channels.insert(channel_id);
            }

            BusMsg::Bolt(bolt::Messages::FundingSigned(bolt::FundingSigned {
                channel_id, ..
            }))
            | BusMsg::Bolt(bolt::Messages::FundingLocked(bolt::FundingLocked {
                channel_id, ..
            }))
            | BusMsg::Bolt(bolt::Messages::UpdateAddHtlc(bolt::UpdateAddHtlc {
                channel_id, ..
            }))
            | BusMsg::Bolt(bolt::Messages::UpdateFulfillHtlc(bolt::UpdateFulfillHtlc {
                channel_id,
                ..
            }))
            | BusMsg::Bolt(bolt::Messages::UpdateFailHtlc(bolt::UpdateFailHtlc {
                channel_id,
                ..
            }))
            | BusMsg::Bolt(bolt::Messages::UpdateFailMalformedHtlc(
                bolt::UpdateFailMalformedHtlc { channel_id, .. },
            )) => {
                let channeld: ServiceId = (*channel_id).into();
                endpoints.send_to(ServiceBus::Msg, self.identity(), channeld, request)?;
            }

            BusMsg::Bolt(message) => {
                // TODO:
                //  1. Check permissions
                //  2. Forward to the corresponding daemon
                debug!("Got peer LN P2P message {}", message);
            }

            wrong_msg => {
                error!("Request is not supported by the BRIDGE interface");
                return Err(Error::wrong_esb_msg(ServiceBus::Bridge, wrong_msg));
            }
        }
        Ok(())
    }

    fn handle_rpc(
        &mut self,
        endpoints: &mut Endpoints,
        client_id: ClientId,
        message: RpcMsg,
    ) -> Result<(), Error> {
        match message {
            RpcMsg::GetInfo => {
                let peer_info = PeerInfo {
                    local_id: self.local_id,
                    remote_id: self.remote_id.map(|id| vec![id]).unwrap_or_default(),
                    local_socket: self.local_socket,
                    remote_socket: vec![self.remote_socket],
                    uptime: SystemTime::now()
                        .duration_since(self.started)
                        .unwrap_or_else(|_| Duration::from_secs(0)),
                    since: self
                        .started
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_else(|_| Duration::from_secs(0))
                        .as_secs(),
                    messages_sent: self.messages_sent,
                    messages_received: self.messages_received,
                    channels: self
                        .channels
                        .iter()
                        .copied()
                        .map(bolt::ActiveChannelId::as_slice32)
                        .collect(),
                    connected: !self.connect,
                    awaits_pong: self.awaited_pong.is_some(),
                };
                self.send_rpc(endpoints, client_id, peer_info)?;
            }

            wrong_msg => {
                error!("Request is not supported by the RPC interface");
                return Err(Error::wrong_esb_msg(ServiceBus::Rpc, &wrong_msg));
            }
        }

        Ok(())
    }

    fn ping(&mut self) -> Result<(), Error> {
        trace!("Sending ping to the remote peer");
        if self.awaited_pong.is_some() {
            warn!(
                "Peer {}@{} ignores our ping. Are we banned?",
                self.remote_id.expect("peer id is known at this stage"),
                self.remote_socket
            );
        }
        let mut rng = rand::thread_rng();
        let len: u16 = rng.gen_range(4, 32);
        let mut noise = vec![0u8; len as usize];
        rng.fill_bytes(&mut noise);
        let pong_size = rng.gen_range(4, 32);
        self.messages_sent += 1;
        self.sender
            .send_message(bolt::Messages::Ping(bolt::Ping { ignored: noise.into(), pong_size }))?;
        self.awaited_pong = Some(pong_size);
        Ok(())
    }

    fn pong(&mut self, pong_size: u16) -> Result<(), Error> {
        trace!("Replying with pong to the remote peer");
        let mut noise = vec![0u8; pong_size as usize];
        let mut rng = rand::thread_rng();
        for byte in &mut noise {
            *byte = rng.gen();
        }
        self.messages_sent += 1;
        self.sender.send_message(bolt::Messages::Pong(noise.into()))?;
        Ok(())
    }
}
