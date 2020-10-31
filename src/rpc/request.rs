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

use lnpbp::lnp::{
    message, rpc_connection, Messages, NodeAddr, RemoteSocketAddr,
};

use crate::ServiceId;

#[derive(Clone, Debug, Display, LnpApi)]
#[lnp_api(encoding = "strict")]
#[non_exhaustive]
pub enum Request {
    #[lnp_api(type = 0)]
    #[display("hello()")]
    Hello,

    #[lnp_api(type = 1)]
    #[display("lnpwp({_0})")]
    LnpwpMessage(Messages),

    // Can be issued from `cli` to `lnpd`
    #[lnp_api(type = 2)]
    #[display("connect()")]
    Listen(RemoteSocketAddr),

    // Can be issued from `cli` to `lnpd`
    #[lnp_api(type = 3)]
    #[display("connect()")]
    ConnectPeer(NodeAddr),

    // Can be issued from `cli` to a specific `connectiond`
    #[lnp_api(type = 4)]
    #[display("ping_peer()")]
    PingPeer,

    // Can be issued from `cli` to `lnpd`
    #[lnp_api(type = 5)]
    #[display("create_channel_with(...)")]
    OpenChannelWith(ChannelParams),

    #[lnp_api(type = 6)]
    #[display("accept_channel_from(...)")]
    AcceptChannelFrom(ChannelParams),
}

impl rpc_connection::Request for Request {}

#[derive(Clone, PartialEq, Eq, Debug, Display, StrictEncode, StrictDecode)]
#[display(Debug)]
pub struct ChannelParams {
    pub channel_req: message::OpenChannel,
    pub connectiond: ServiceId,
}
