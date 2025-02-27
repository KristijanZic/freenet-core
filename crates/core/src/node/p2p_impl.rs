use std::sync::Arc;

use libp2p::{
    core::{
        muxing,
        transport::{self, upgrade},
    },
    dns,
    identity::Keypair,
    noise, tcp, yamux, PeerId, Transport,
};

use super::{
    client_event_handling,
    conn_manager::{p2p_protoc::P2pConnManager, EventLoopNotifications},
    join_ring_request, EventLogRegister, PeerKey,
};
use crate::{
    client_events::combinator::ClientEventsCombinator,
    config::{self, GlobalExecutor},
    contract::{
        self, ClientResponsesSender, ContractHandler, ExecutorToEventLoopChannel,
        NetworkEventListenerHalve,
    },
    node::NodeBuilder,
    ring::Ring,
    util::IterExt,
};

use super::OpManager;

pub(super) struct NodeP2P {
    pub(crate) peer_key: PeerKey,
    pub(crate) op_manager: Arc<OpManager>,
    notification_channel: EventLoopNotifications,
    pub(super) conn_manager: P2pConnManager,
    is_gateway: bool,
    executor_listener: ExecutorToEventLoopChannel<NetworkEventListenerHalve>,
    cli_response_sender: ClientResponsesSender,
}

impl NodeP2P {
    pub(super) async fn run_node(mut self) -> Result<(), anyhow::Error> {
        // start listening in case this is a listening node (gateway) and join the ring
        if self.is_gateway {
            self.conn_manager.listen_on()?;
        }

        if !self.is_gateway {
            if let Some(gateway) = self.conn_manager.gateways.iter().shuffle().take(1).next() {
                join_ring_request(
                    None,
                    self.peer_key,
                    gateway,
                    &self.op_manager,
                    &mut self.conn_manager.bridge,
                )
                .await?;
            } else {
                anyhow::bail!("requires at least one gateway");
            }
        }

        // start the p2p event loop
        self.conn_manager
            .run_event_listener(
                self.op_manager.clone(),
                self.notification_channel,
                self.executor_listener,
                self.cli_response_sender,
            )
            .await
    }

    pub(crate) async fn build<CH, const CLIENTS: usize, EL>(
        builder: NodeBuilder<CLIENTS>,
        event_listener: EL,
        ch_builder: CH::Builder,
    ) -> Result<NodeP2P, anyhow::Error>
    where
        CH: ContractHandler + Send + Sync + 'static,
        EL: EventLogRegister + Clone,
    {
        let peer_key = PeerKey::from(builder.local_key.public());
        let gateways = builder.get_gateways()?;

        let ring = Ring::new::<CLIENTS, EL>(&builder, &gateways)?;
        let (notification_channel, notification_tx) = EventLoopNotifications::channel();
        let (ch_outbound, ch_inbound) = contract::contract_handler_channel();
        let (client_responses, cli_response_sender) = contract::ClientResponses::channel();
        let op_storage = Arc::new(OpManager::new(ring, notification_tx, ch_outbound));
        let (executor_listener, executor_sender) = contract::executor_channel(op_storage.clone());
        let contract_handler = CH::build(ch_inbound, executor_sender, ch_builder)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        let conn_manager = {
            let transport = Self::config_transport(&builder.local_key)?;
            P2pConnManager::build(transport, &builder, op_storage.clone(), event_listener)?
        };

        GlobalExecutor::spawn(contract::contract_handling(contract_handler));
        let clients = ClientEventsCombinator::new(builder.clients);
        GlobalExecutor::spawn(client_event_handling(
            op_storage.clone(),
            clients,
            client_responses,
        ));

        Ok(NodeP2P {
            peer_key,
            conn_manager,
            notification_channel,
            op_manager: op_storage,
            is_gateway: builder.location.is_some(),
            executor_listener,
            cli_response_sender,
        })
    }

    /// Capabilities built into the transport by default:
    ///
    /// - TCP/IP handling over Tokio streams.
    /// - DNS when dialing peers.
    /// - Authentication and encryption via [Noise](https://github.com/libp2p/specs/tree/master/noise) protocol.
    /// - Compression using Deflate.
    /// - Multiplexing using [Yamux](https://github.com/hashicorp/yamux/blob/master/spec.md).
    fn config_transport(
        local_key: &Keypair,
    ) -> std::io::Result<transport::Boxed<(PeerId, muxing::StreamMuxerBox)>> {
        let tcp = tcp::tokio::Transport::new(tcp::Config::new().nodelay(true).port_reuse(true));
        let with_dns = dns::tokio::Transport::system(tcp)?;
        Ok(with_dns
            .upgrade(upgrade::Version::V1)
            .authenticate(noise::Config::new(local_key).unwrap())
            .multiplex(yamux::Config::default())
            .timeout(config::PEER_TIMEOUT)
            .map(|(peer, muxer), _| (peer, muxing::StreamMuxerBox::new(muxer)))
            .boxed())
    }
}

#[cfg(test)]
mod test {
    use std::{net::Ipv4Addr, time::Duration};

    use super::super::conn_manager::p2p_protoc::NetEvent;
    use super::*;
    use crate::{
        client_events::test::MemoryEventsGen,
        config::GlobalExecutor,
        contract::MemoryContractHandler,
        node::{event_log, tests::get_free_port, InitPeerNode},
        ring::Location,
    };

    use futures::StreamExt;
    use libp2p::swarm::SwarmEvent;
    use tokio::sync::watch::channel;

    /// Ping test event loop
    async fn ping_ev_loop(peer: &mut NodeP2P) -> Result<(), ()> {
        loop {
            let ev = tokio::time::timeout(
                Duration::from_secs(30),
                peer.conn_manager.swarm.select_next_some(),
            );
            match ev.await {
                Ok(SwarmEvent::Behaviour(NetEvent::Ping(ping))) => {
                    if ping.result.is_ok() {
                        tracing::info!("ping done @ {}", peer.peer_key);
                        return Ok(());
                    }
                }
                Ok(other) => {
                    tracing::debug!("{:?}", other)
                }
                Err(_) => {
                    return Err(());
                }
            }
        }
    }

    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ping() -> Result<(), ()> {
        let peer1_port = get_free_port().unwrap();
        let peer1_key = Keypair::generate_ed25519();
        let peer1_id: PeerId = peer1_key.public().into();
        let peer1_config = InitPeerNode::new(peer1_id, Location::random())
            .listening_ip(Ipv4Addr::LOCALHOST)
            .listening_port(peer1_port);

        let peer2_key = Keypair::generate_ed25519();
        let peer2_id: PeerId = peer2_key.public().into();

        let (_, receiver1) = channel((0, PeerKey::from(peer1_id)));
        let (_, receiver2) = channel((0, PeerKey::from(peer2_id)));

        // Start up the initial node.
        GlobalExecutor::spawn(async move {
            let user_events = MemoryEventsGen::new(receiver1, PeerKey::from(peer1_id));
            let mut config = NodeBuilder::new([Box::new(user_events)]);
            config
                .with_ip(Ipv4Addr::LOCALHOST)
                .with_port(peer1_port)
                .with_key(peer1_key);
            let mut peer1 = Box::new(
                NodeP2P::build::<MemoryContractHandler, 1, _>(
                    config,
                    event_log::TestEventListener::new(),
                    "ping-listener".into(),
                )
                .await?,
            );
            peer1.conn_manager.listen_on()?;
            ping_ev_loop(&mut peer1).await.unwrap();
            Ok::<_, anyhow::Error>(())
        });

        // Start up the dialing node
        let dialer = GlobalExecutor::spawn(async move {
            let user_events = MemoryEventsGen::new(receiver2, PeerKey::from(peer2_id));
            let mut config = NodeBuilder::new([Box::new(user_events)]);
            config.add_gateway(peer1_config.clone());
            let mut peer2 = NodeP2P::build::<MemoryContractHandler, 1, _>(
                config,
                event_log::TestEventListener::new(),
                "ping-dialer".into(),
            )
            .await
            .unwrap();
            // wait a bit to make sure the first peer is up and listening
            tokio::time::sleep(Duration::from_millis(10)).await;
            peer2
                .conn_manager
                .swarm
                .dial(peer1_config.addr.unwrap())
                .map_err(|_| ())?;
            ping_ev_loop(&mut peer2).await
        });

        dialer.await.map_err(|_| ())?
    }
}
