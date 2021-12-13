// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
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

use std::convert::TryFrom;
use std::time::SystemTime;

use bitcoin::hashes::{sha256, Hash, HashEngine};
use bitcoin::secp256k1::{All, Secp256k1};
use bitcoin::{secp256k1, OutPoint};
#[cfg(feature = "rgb")]
use bp::seals::OutpointReveal;
#[cfg(feature = "rgb")]
use internet2::zmqsocket::{self, ZmqSocketAddr, ZmqType};
use internet2::NodeAddr;
#[cfg(feature = "rgb")]
use internet2::{session, CreateUnmarshaller, Session, Unmarshall, Unmarshaller};
use lnp::bolt::extensions::{HtlcKnown, HtlcSecret};
use lnp::bolt::{self, AssetsBalance, Lifecycle, ScriptGenerators};
use lnp::channel::Channel;
use lnp::p2p::legacy::{
    AcceptChannel, ChannelId, FundingCreated, FundingLocked, FundingSigned, Messages as LnMsg,
    OpenChannel, TempChannelId,
};
#[cfg(feature = "rgb")]
use lnpbp::chain::AssetId;
use lnpbp::chain::Chain;
use microservices::esb::{self, Handler};
#[cfg(feature = "rgb")]
use rgb::Consignment;
use wallet::scripts::PubkeyScript;

use super::storage::{self, Driver};
use crate::channeld::state_machines::ChannelStateMachine;
use crate::i9n::ctl::CtlMsg;
use crate::i9n::rpc::Failure;
use crate::i9n::{ctl as request, BusMsg, ServiceBus};
use crate::{Config, CtlServer, Endpoints, Error, LogStyle, Service, ServiceId};

pub fn run(
    config: Config,
    channel_id: ChannelId,
    #[cfg(feature = "rgb")] rgb20_socket_addr: ZmqSocketAddr,
) -> Result<(), Error> {
    #[cfg(feature = "rgb")]
    let rgb20_rpc =
        session::Raw::with_zmq_unencrypted(ZmqType::Req, &rgb20_socket_addr, None, None)?;
    #[cfg(feature = "rgb")]
    let rgb_unmarshaller = rgb_node::rpc::Reply::create_unmarshaller();

    let runtime = Runtime {
        identity: ServiceId::Channel(channel_id),
        peer_service: ServiceId::Loopback,
        chain: config.chain.clone(),
        secp: Secp256k1::new(),
        state_machine: default!(),
        channel: default!(), // TODO: use node configuration to provide custom policy & parameters
        channel_id: zero!(),
        temporary_channel_id: channel_id.into(),
        state: default!(),
        local_capacity: 0,
        remote_capacity: 0,
        local_balances: zero!(),
        remote_balances: zero!(),
        funding_outpoint: default!(),
        remote_peer: None,
        started: SystemTime::now(),
        commitment_number: 0,
        total_payments: 0,
        pending_payments: 0,
        common_params: default!(),
        local_params: default!(),
        remote_params: default!(),
        local_keys: dumb!(),
        remote_keys: dumb!(),
        offered_htlc: empty!(),
        received_htlc: empty!(),
        is_originator: false,
        obscuring_factor: 0,
        enquirer: None,
        #[cfg(feature = "rgb")]
        rgb20_rpc,
        #[cfg(feature = "rgb")]
        rgb_unmarshaller,
        storage: Box::new(storage::DiskDriver::init(
            channel_id,
            Box::new(storage::DiskConfig { path: Default::default() }),
        )?),
    };

    Service::run(config, runtime, false)
}

pub struct Runtime {
    identity: ServiceId,
    pub(crate) peer_service: ServiceId,
    chain: Chain,

    secp: Secp256k1<All>,

    pub(crate) state_machine: ChannelStateMachine,
    pub(crate) channel: Channel<bolt::ExtensionId>,

    // From here till the `enqueror` all parameters should be removed since they are part of
    // `channel` now
    channel_id: ChannelId,
    temporary_channel_id: TempChannelId,
    state: Lifecycle,
    local_capacity: u64,
    remote_capacity: u64,
    local_balances: AssetsBalance,
    remote_balances: AssetsBalance,
    funding_outpoint: OutPoint,
    remote_peer: Option<NodeAddr>,
    started: SystemTime,
    commitment_number: u64,
    total_payments: u64,
    pending_payments: u16,
    common_params: bolt::CommonParams,
    local_params: bolt::PeerParams,
    remote_params: bolt::PeerParams,
    local_keys: bolt::Keyset,
    remote_keys: bolt::Keyset,

    offered_htlc: Vec<HtlcKnown>,
    received_htlc: Vec<HtlcSecret>,

    is_originator: bool,
    obscuring_factor: u64,

    // TODO: Refactor to use ClientId
    enquirer: Option<ServiceId>,
    #[cfg(feature = "rgb")]
    rgb20_rpc: session::Raw<session::PlainTranscoder, zmqsocket::Connection>,
    #[cfg(feature = "rgb")]
    rgb_unmarshaller: Unmarshaller<rgb_node::rpc::Reply>,

    #[allow(dead_code)]
    storage: Box<dyn storage::Driver>,
}

impl CtlServer for Runtime {
    #[inline]
    fn enquirer(&self) -> Option<ServiceId> { self.enquirer.clone() }
}

impl Runtime {
    #[inline]
    pub fn channel_capacity(&self) -> u64 { self.local_capacity + self.remote_capacity }
}

impl esb::Handler<ServiceBus> for Runtime {
    type Request = BusMsg;
    type Address = ServiceId;
    type Error = Error;

    fn identity(&self) -> ServiceId { self.identity.clone() }

    fn handle(
        &mut self,
        endpoints: &mut Endpoints,
        bus: ServiceBus,
        source: ServiceId,
        message: BusMsg,
    ) -> Result<(), Self::Error> {
        match (bus, message, source) {
            (ServiceBus::Msg, BusMsg::Ln(msg), ServiceId::Peer(remote_peer)) => {
                self.handle_p2p(endpoints, remote_peer, msg)
            }
            (ServiceBus::Msg, BusMsg::Ln(_), service) => {
                unreachable!("channeld received peer message not from a peerd but from {}", service)
            }
            (ServiceBus::Ctl, BusMsg::Ctl(msg), source) => self.handle_ctl(endpoints, source, msg),
            (ServiceBus::Rpc, ..) => unreachable!("peer daemon must not bind to RPC interface"),
            (bus, msg, _) => Err(Error::wrong_rpc_msg(bus, &msg)),
        }
    }

    fn handle_err(&mut self, _: esb::Error) -> Result<(), esb::Error> {
        // We do nothing and do not propagate error; it's already being reported
        // with `error!` macro by the controller. If we propagate error here
        // this will make whole daemon panic
        Ok(())
    }
}

impl Runtime {
    #[cfg(feature = "rgb")]
    fn request_rbg20(
        &mut self,
        request: rgb_node::rpc::fungible::Request,
    ) -> Result<rgb_node::rpc::Reply, Error> {
        let data = request.serialize();
        self.rgb20_rpc.send_raw_message(&data)?;
        let raw = self.rgb20_rpc.recv_raw_message()?;
        let reply = &*self.rgb_unmarshaller.unmarshall(&raw)?;
        if let rgb_node::rpc::Reply::Failure(failure) = reply {
            error!("{} {}", "RGB Node reported failure:".err(), failure.err())
        }
        Ok(reply.clone())
    }

    pub fn send_p2p(&self, endpoints: &mut Endpoints, message: LnMsg) -> Result<(), esb::Error> {
        endpoints.send_to(
            ServiceBus::Msg,
            self.identity(),
            self.peer_service.clone(),
            BusMsg::Ln(message),
        )?;
        Ok(())
    }

    fn handle_p2p(
        &mut self,
        endpoints: &mut Endpoints,
        remote_peer: NodeAddr,
        message: LnMsg,
    ) -> Result<(), Error> {
        match message {
            LnMsg::OpenChannel(_) => {
                // TODO: Support repeated messages according to BOLT-2 requirements
                // if the connection has been re-established after receiving a previous
                // open_channel, BUT before receiving a funding_created message:
                //     accept a new open_channel message.
                //     discard the previous open_channel message.
                warn!(
                    "Got `open_channel` P2P message from {}, which is unexpected: the channel \
                     creation was already requested before",
                    remote_peer
                );
            }

            LnMsg::AcceptChannel(accept_channel) => {
                self.state = Lifecycle::Accepted;

                self.channel_accepted(endpoints, &accept_channel, &ServiceId::Peer(remote_peer))
                    .map_err(|err| {
                        self.report_failure(endpoints, Failure {
                            code: 0, // TODO: Create error type system
                            info: err.to_string(),
                        })
                    })?;

                // Construct funding output scriptPubkey
                let remote_pk = accept_channel.funding_pubkey;
                let local_pk = self.local_keys.funding_pubkey;
                trace!("Generating script pubkey from local {} and remote {}", local_pk, remote_pk);
                let script_pubkey =
                    PubkeyScript::ln_funding(self.channel_capacity(), local_pk, remote_pk);
                trace!("Funding script: {}", script_pubkey);
                if let Some(addr) = bitcoin::Network::try_from(&self.chain)
                    .ok()
                    .and_then(|network| script_pubkey.address(network))
                {
                    debug!("Funding address: {}", addr);
                } else {
                    error!(
                        "{} {}",
                        "Unable to generate funding address for the current network ".err(),
                        self.chain.err()
                    )
                }

                /*
                // Ignoring possible error here: do not want to halt the channel just because the
                // client disconnected
                let _ = self.send_ctl(
                    endpoints,
                    &self.enquirer.clone(),
                    CtlMsg::ChannelFunding(script_pubkey),
                );
                 */
            }

            LnMsg::FundingCreated(funding_created) => {
                self.state = Lifecycle::Funding;

                let funding_signed = self.funding_created(endpoints, funding_created)?;

                self.send_p2p(endpoints, LnMsg::FundingSigned(funding_signed))?;

                self.state = Lifecycle::Funded;

                // Ignoring possible error here: do not want to
                // halt the channel just because the client disconnected
                let msg = format!("{} both signatures present", "Channel funded:".ended());
                info!("{}", msg);
                let _ = self.report_progress(endpoints, msg);
            }

            LnMsg::FundingSigned(_funding_signed) => {
                // TODO:
                //      1. Get commitment tx
                //      2. Verify signature
                //      3. Save signature/commitment tx
                //      4. Send funding locked request

                self.state = Lifecycle::Funded;

                // Ignoring possible error here: do not want to
                // halt the channel just because the client disconnected
                let msg = format!("{} both signatures present", "Channel funded:".ended());
                info!("{}", msg);
                let _ = self.report_progress(endpoints, msg);

                let funding_locked = FundingLocked {
                    channel_id: self.channel_id,
                    next_per_commitment_point: self.local_keys.first_per_commitment_point,
                };

                self.send_p2p(endpoints, LnMsg::FundingLocked(funding_locked))?;

                self.state = Lifecycle::Active;
                self.local_capacity = self.channel.constructor().local_amount();

                // Ignoring possible error here: do not want to
                // halt the channel just because the client disconnected
                let msg = format!("{} transaction confirmed", "Channel active:".ended());
                info!("{}", msg);
                let _ = self.report_success(endpoints, Some(msg));
            }

            LnMsg::FundingLocked(_funding_locked) => {
                self.state = Lifecycle::Locked;

                // TODO:
                //      1. Change the channel state
                //      2. Do something with per-commitment point

                self.state = Lifecycle::Active;
                self.remote_capacity = self.channel.constructor().remote_amount();

                // Ignoring possible error here: do not want to
                // halt the channel just because the client disconnected
                let msg = format!("{} transaction confirmed", "Channel active:".ended());
                info!("{}", msg);
                let _ = self.report_success(endpoints, Some(msg));
            }

            LnMsg::UpdateAddHtlc(_update_add_htlc) => {
                // let _commitment_signed = self.htlc_receive(endpoints, update_add_htlc)?;
            }

            LnMsg::CommitmentSigned(_commitment_signed) => {}

            LnMsg::RevokeAndAck(_revoke_ack) => {}

            #[cfg(feature = "rgb")]
            LnMsg::AssignFunds(assign_req) => {
                self.refill(
                    endpoints,
                    assign_req.consignment,
                    assign_req.outpoint,
                    assign_req.blinding,
                    false,
                )?;

                // TODO: Re-sign the commitment and return to the remote peer
            }

            _ => {
                // Ignore the rest of LN peer messages
            }
        }
        Ok(())
    }

    fn handle_ctl(
        &mut self,
        senders: &mut Endpoints,
        source: ServiceId,
        request: CtlMsg,
    ) -> Result<(), Error> {
        // RPC control requests are sent by either clients or lnpd daemon and used to initiate one
        // of channel workflows and to request information about the channel state.
        match request {
            // Proposing remote peer to open a channel
            CtlMsg::OpenChannelWith(open_channel_with) => {
                let remote_peer = open_channel_with.remote_peer.clone();
                self.enquirer = open_channel_with.report_to.clone();
                self.propose_channel(senders, open_channel_with)?;
                // Updating state only if the request was processed
                self.peer_service = ServiceId::Peer(remote_peer.clone());
                self.remote_peer = Some(remote_peer);
            }

            // Processing remote request to open a channel
            CtlMsg::AcceptChannelFrom(request::AcceptChannelFrom { ref remote_peer, .. }) => {
                self.enquirer = None;
                let remote_peer = remote_peer.clone();
                if self.process(senders, source, request)? {
                    // Updating state only if the request was processed
                    self.peer_service = ServiceId::Peer(remote_peer.clone());
                    self.remote_peer = Some(remote_peer);
                }
            }

            /*
            // TODO: delete
            Request::FundingConstructed(funding_outpoint) => {
                let funding_created = self.fund_channel(senders, funding_outpoint)?;
                self.state = Lifecycle::Funding;
                self.send_peer(senders, Messages::FundingCreated(funding_created))?;
            }

            #[cfg(feature = "rgb")]
            Request::RefillChannel(refill_req) => {
                self.enquirer = source.into();

                self.refill(
                    senders,
                    refill_req.consignment.clone(),
                    refill_req.outpoint,
                    refill_req.blinding,
                    true,
                )?;

                let assign_funds = AssignFunds {
                    channel_id: self.channel_id,
                    consignment: refill_req.consignment,
                    outpoint: refill_req.outpoint,
                    blinding: refill_req.blinding,
                };

                self.send_peer(senders, Messages::AssignFunds(assign_funds))?;
            }

            Request::Transfer(transfer_req) => {
                self.enquirer = source.into();

                let update_add_htlc = self.transfer(senders, transfer_req)?;

                self.send_peer(senders, Messages::UpdateAddHtlc(update_add_htlc))?;
            }

            Request::GetInfo => {
                fn bmap<T>(remote_peer: &Option<NodeAddr>, v: &T) -> BTreeMap<NodeAddr, T>
                where
                    T: Clone,
                {
                    remote_peer
                        .as_ref()
                        .map(|p| bmap! { p.clone() => v.clone() })
                        .unwrap_or_default()
                }

                let channel_id =
                    if self.channel_id == zero!() { None } else { Some(self.channel_id) };
                let info = ChannelInfo {
                    channel_id,
                    temporary_channel_id: self.temporary_channel_id,
                    state: self.state,
                    local_capacity: self.local_capacity,
                    remote_capacities: bmap(&self.remote_peer, &self.remote_capacity),
                    assets: self.local_balances.keys().cloned().collect(),
                    local_balances: self.local_balances.clone(),
                    remote_balances: bmap(&self.remote_peer, &self.remote_balances),
                    funding_outpoint: self.funding_outpoint,
                    remote_peers: self.remote_peer.clone().map(|p| vec![p]).unwrap_or_default(),
                    uptime: SystemTime::now()
                        .duration_since(self.started)
                        .unwrap_or(Duration::from_secs(0)),
                    since: self
                        .started
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or(Duration::from_secs(0))
                        .as_secs(),
                    commitment_updates: self.commitment_number,
                    total_payments: self.total_payments,
                    pending_payments: self.pending_payments,
                    is_originator: self.is_originator,
                    params: self.params,
                    local_keys: self.local_keys.clone(),
                    remote_keys: bmap(&self.remote_peer, &self.remote_keys),
                };
                self.send_ctl(senders, source, Request::ChannelInfo(info))?;
            }
             */
            _ => {
                error!("Request is not supported by the CTL interface");
                return Err(Error::wrong_rpc_msg(ServiceBus::Ctl, &request));
            }
        }
        Ok(())
    }
}

impl Runtime {
    pub fn update_channel_id(&mut self, senders: &mut Endpoints) -> Result<(), Error> {
        // Update channel id!
        self.channel_id =
            ChannelId::with(self.funding_outpoint.txid, self.funding_outpoint.vout as u16);
        debug!("Updating channel id to {}", self.channel_id);
        self.send_ctl(senders, ServiceId::Lnpd, CtlMsg::UpdateChannelId(self.channel_id))?;
        self.send_ctl(
            senders,
            self.peer_service.clone(),
            CtlMsg::UpdateChannelId(self.channel_id),
        )?;
        // self.identity = self.channel_id.into();
        let msg = format!("{} set to {}", "Channel ID".ended(), self.channel_id.ender());
        info!("{}", msg);
        let _ = self.report_progress(senders, msg);

        Ok(())
    }

    pub fn accept_channel(
        &mut self,
        senders: &mut Endpoints,
        channel_req: &OpenChannel,
        peerd: &ServiceId,
    ) -> Result<AcceptChannel, bolt::PolicyError> {
        let msg = format!(
            "{} with temp id {:#} from remote peer {}",
            "Accepting channel".promo(),
            channel_req.temporary_channel_id.promoter(),
            peerd.promoter()
        );
        info!("{}", msg);

        // Ignoring possible reporting errors here and after: do not want to
        // halt the channel just because the client disconnected
        let _ = self.report_progress(senders, msg);

        self.is_originator = false;
        self.local_params = bolt::PeerParams::from(channel_req);
        self.remote_keys = bolt::Keyset::from(channel_req);

        let dumb_key = secp256k1::PublicKey::from_secret_key(&self.secp, &secp256k1::key::ONE_KEY);
        let accept_channel = AcceptChannel {
            temporary_channel_id: channel_req.temporary_channel_id,
            dust_limit_satoshis: channel_req.dust_limit_satoshis,
            max_htlc_value_in_flight_msat: channel_req.max_htlc_value_in_flight_msat,
            channel_reserve_satoshis: channel_req.channel_reserve_satoshis,
            htlc_minimum_msat: channel_req.htlc_minimum_msat,
            minimum_depth: 3, // TODO: take from config options
            to_self_delay: channel_req.to_self_delay,
            max_accepted_htlcs: channel_req.max_accepted_htlcs,
            funding_pubkey: dumb_key,
            revocation_basepoint: dumb_key,
            payment_point: dumb_key,
            delayed_payment_basepoint: dumb_key,
            htlc_basepoint: dumb_key,
            first_per_commitment_point: dumb_key,
            shutdown_scriptpubkey: None,
            channel_type: None,
            unknown_tlvs: none!(),
        };

        self.local_keys = bolt::Keyset::from(&accept_channel);

        Ok(accept_channel)
    }

    pub fn channel_accepted(
        &mut self,
        senders: &mut Endpoints,
        accept_channel: &AcceptChannel,
        peerd: &ServiceId,
    ) -> Result<(), bolt::PolicyError> {
        info!(
            "Channel {:#} {} by the remote peer {}",
            accept_channel.temporary_channel_id.ender(),
            "was accepted".ended(),
            peerd.ender()
        );
        // Ignoring possible reporting errors here and after: do not want to
        // halt the channel just because the client disconnected
        let _ = self.report_progress(senders, "Channel was accepted by the remote peer");

        let msg = format!(
            "{} returned parameters for the channel {:#}",
            "Verifying".promo(),
            accept_channel.temporary_channel_id.promoter()
        );
        info!("{}", msg);

        // TODO: Add a reasonable min depth bound
        self.remote_params = bolt::PeerParams::from(accept_channel);
        self.remote_keys = bolt::Keyset::from(accept_channel);

        let msg = format!(
            "Channel {:#} is {}",
            accept_channel.temporary_channel_id.ender(),
            "ready for funding".ended()
        );
        info!("{}", msg);
        let _ = self.report_success(senders, Some(msg));

        Ok(())
    }

    pub fn fund_channel(
        &mut self,
        senders: &mut Endpoints,
        funding_outpoint: OutPoint,
    ) -> Result<FundingCreated, Error> {
        info!("{} {}", "Funding channel".promo(), self.temporary_channel_id.promoter());
        let _ = self
            .report_progress(senders, format!("Funding channel {:#}", self.temporary_channel_id));

        self.funding_outpoint = funding_outpoint;
        self.funding_update(senders)?;

        let signature = self.sign_funding();
        let funding_created = FundingCreated {
            temporary_channel_id: self.temporary_channel_id,
            funding_txid: self.funding_outpoint.txid,
            funding_output_index: self.funding_outpoint.vout as u16,
            signature,
        };
        trace!("Prepared funding_created: {:?}", funding_created);

        let msg = format!(
            "{} for channel {:#}. Awaiting for remote node signature.",
            "Funding created".ended(),
            self.channel_id.ender()
        );
        info!("{}", msg);
        let _ = self.report_progress(senders, msg);

        Ok(funding_created)
    }

    pub fn funding_created(
        &mut self,
        senders: &mut Endpoints,
        funding_created: FundingCreated,
    ) -> Result<FundingSigned, Error> {
        info!("{} {}", "Accepting channel funding".promo(), self.temporary_channel_id.promoter());
        let _ = self.report_progress(
            senders,
            format!("Accepting channel funding {:#}", self.temporary_channel_id),
        );

        self.funding_outpoint = OutPoint {
            txid: funding_created.funding_txid,
            vout: funding_created.funding_output_index as u32,
        };
        // TODO: Save signature!
        self.funding_update(senders)?;

        let signature = self.sign_funding();
        let funding_signed = FundingSigned { channel_id: self.channel_id, signature };
        trace!("Prepared funding_signed: {:?}", funding_signed);

        let msg = format!(
            "{} for channel {:#}. Awaiting for funding tx mining.",
            "Funding signed".ended(),
            self.channel_id.ender()
        );
        info!("{}", msg);
        let _ = self.report_progress(senders, msg);

        Ok(funding_signed)
    }

    pub fn funding_update(&mut self, senders: &mut Endpoints) -> Result<(), Error> {
        let mut engine = sha256::Hash::engine();
        if self.is_originator {
            engine.input(&self.local_keys.payment_basepoint.serialize());
            engine.input(&self.remote_keys.payment_basepoint.serialize());
        } else {
            engine.input(&self.remote_keys.payment_basepoint.serialize());
            engine.input(&self.local_keys.payment_basepoint.serialize());
        }
        let obscuring_hash = sha256::Hash::from_engine(engine);
        trace!("Obscuring hash: {}", obscuring_hash);

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&obscuring_hash[24..]);
        self.obscuring_factor = u64::from_be_bytes(buf);
        trace!("Obscuring factor: {:#016x}", self.obscuring_factor);
        self.commitment_number = 0;

        self.update_channel_id(senders)?;

        Ok(())
    }

    pub fn sign_funding(&mut self) -> secp256k1::Signature {
        todo!()
        /*
        // We are doing counterparty's transaction!
        let mut cmt_tx = Transaction::ln_cmt_base(
            self.remote_capacity,
            self.local_capacity,
            self.commitment_number,
            self.obscuring_factor,
            self.funding_outpoint,
            self.local_keys.payment_basepoint,
            self.local_keys.revocation_basepoint,
            self.remote_keys.delayed_payment_basepoint,
            self.local_params.to_self_delay,
        );
        trace!("Counterparty's commitment tx: {:?}", cmt_tx);

        let mut sig_hasher = SigHashCache::new(&mut cmt_tx);
        let sighash = sig_hasher.signature_hash(
            0,
            &PubkeyScript::ln_funding(
                self.channel_capacity(),
                self.local_keys.funding_pubkey,
                self.remote_keys.funding_pubkey,
            )
            .into(),
            self.channel_capacity(),
            SigHashType::All,
        );

        let sign_msg = secp256k1::Message::from_slice(&sighash[..])
            .expect("Sighash size always match requirements");
        let signature = self.local_node.sign(&self.secp, &sign_msg);
        trace!("Commitment transaction signature created");

        signature*/
    }

    /* TODO: delete
    pub fn transfer(
        &mut self,
        senders: &mut Senders,
        transfer_req: ctl::Transfer,
    ) -> Result<UpdateAddHtlc, Error> {
        let available = if let Some(asset_id) = transfer_req.asset {
            self.local_balances.get(&asset_id).copied().unwrap_or(0)
        } else {
            self.local_capacity
        };

        if available < transfer_req.amount {
            Err(Error::Other(s!("You do not have required amount of the asset")))?
        }

        info!(
            "{} {} {} to the remote peer",
            "Transferring".promo(),
            transfer_req.amount.promoter(),
            transfer_req.asset.map(|a| a.to_string()).unwrap_or(s!("msat")).promoter(),
        );

        let preimage = HashPreimage::random();
        let payment_hash = preimage.into();
        let htlc = HtlcKnown {
            preimage,
            id: self.total_payments,
            cltv_expiry: 0,
            amount: transfer_req.amount,
            asset_id: transfer_req.asset,
        };
        trace!("Generated HTLC: {:?}", htlc);
        self.offered_htlc.push(htlc);

        let update_add_htlc = UpdateAddHtlc {
            channel_id: self.channel_id,
            htlc_id: htlc.id,
            amount_msat: transfer_req.amount,
            payment_hash,
            cltv_expiry: htlc.cltv_expiry,
            onion_routing_packet: dumb!(), // TODO: Generate proper onion packet
            asset_id: transfer_req.asset,
            unknown_tlvs: Default::default(),
        };
        self.total_payments += 1;
        match transfer_req.asset {
            Some(asset_id) => {
                self.local_balances.get_mut(&asset_id).map(|balance| {
                    *balance -= transfer_req.amount;
                });

                let entry = self.remote_balances.entry(asset_id).or_insert(0);
                *entry += transfer_req.amount;
            }
            None => {
                self.local_capacity -= transfer_req.amount;
                self.remote_capacity += transfer_req.amount;
            }
        }

        let msg = format!("{}", "Funding transferred".ended());
        info!("{}", msg);
        let _ = self.report_progress(senders, msg);

        Ok(update_add_htlc)
    }

    #[cfg(feature = "rgb")]
    pub fn refill(
        &mut self,
        senders: &mut Senders,
        consignment: Consignment,
        outpoint: OutPoint,
        blinding: u64,
        refill_originator: bool,
    ) -> Result<(), Error> {
        debug!("Validating consignment with RGB Node ...");
        self.request_rbg20(rgb_node::rpc::fungible::Request::Validate(consignment.clone()))?;

        debug!("Adding consignment to stash via RGB Node ...");
        self.request_rbg20(rgb_node::rpc::fungible::Request::Accept(
            rgb_node::rpc::fungible::AcceptReq {
                consignment: consignment.clone(),
                reveal_outpoints: vec![OutpointReveal {
                    blinding,
                    txid: outpoint.txid,
                    vout: outpoint.vout,
                }],
            },
        ))?;

        debug!("Requesting new balances for {} ...", outpoint);
        match self.request_rbg20(rgb_node::rpc::fungible::Request::Assets(outpoint))? {
            rgb_node::rpc::Reply::OutpointAssets(balances) => {
                for (id, balances) in balances {
                    let asset_id = AssetId::from(id);
                    let balance: u64 = balances.into_iter().sum();
                    info!(
                        "{} {} of {} to balance",
                        "Adding".promo(),
                        balance.promoter(),
                        asset_id.promoter()
                    );
                    let msg =
                        format!("adding {} of {} to balance", balance.ender(), asset_id.ender());
                    let _ = self.report_progress(senders, msg);

                    if refill_originator {
                        self.local_balances.insert(asset_id, balance);
                        self.remote_balances.insert(asset_id, 0);
                    } else {
                        self.remote_balances.insert(asset_id, balance);
                        self.local_balances.insert(asset_id, 0);
                    };
                }
            }
            _ => Err(Error::Other(s!("Unrecognized RGB Node response")))?,
        }

        let _ = self.report_success(senders, Some("transfer completed"));
        Ok(())
    }

    pub fn htlc_receive(
        &mut self,
        _senders: &mut Senders,
        update_add_htlc: UpdateAddHtlc,
    ) -> Result</* message::CommitmentSigned */ (), Error> {
        trace!("Updating HTLCs with {:?}", update_add_htlc);
        // TODO: Use From/To for message <-> Htlc conversion in LNP/BP
        //       Core lib
        let htlc = HtlcSecret {
            amount: update_add_htlc.amount_msat,
            hashlock: update_add_htlc.payment_hash,
            id: update_add_htlc.htlc_id,
            cltv_expiry: update_add_htlc.cltv_expiry,
            asset_id: update_add_htlc.asset_id,
        };
        self.received_htlc.push(htlc);

        let available = if let Some(asset_id) = update_add_htlc.asset_id {
            self.remote_balances.get(&asset_id).copied().unwrap_or(0)
        } else {
            self.remote_capacity
        };

        if available < update_add_htlc.amount_msat {
            Err(Error::Other(s!("Remote node does not have required amount of the asset")))?
        }

        self.total_payments += 1;
        match update_add_htlc.asset_id {
            Some(asset_id) => {
                self.remote_balances.get_mut(&asset_id).map(|balance| {
                    *balance -= update_add_htlc.amount_msat;
                });

                let entry = self.local_balances.entry(asset_id).or_insert(0);
                *entry += update_add_htlc.amount_msat;
            }
            None => {
                self.remote_capacity -= update_add_htlc.amount_msat;
                self.local_capacity += update_add_htlc.amount_msat;
            }
        }

        Ok(())

        // TODO:
        //      1. Generate new commitment tx
        //      2. Generate new transitions and anchor, commit into tx
        //      3. Sign commitment tx
        //      4. Generate HTLCs, tweak etc each of them
        //      3. Send response
    }
     */
}