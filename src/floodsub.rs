use futures::channel::mpsc as channel;
use futures::stream::{FusedStream, Stream};
use tracing::debug;

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use libp2p::core::{Multiaddr, PeerId};
use libp2p::floodsub::{Floodsub, FloodsubConfig, FloodsubEvent, FloodsubMessage, Topic};
use libp2p::swarm::{NetworkBehaviour, NetworkBehaviourAction, PollParameters, THandlerInEvent};

/// Currently a thin wrapper around Floodsub, perhaps supporting both Gossipsub and Floodsub later.
/// Allows single subscription to a topic with only unbounded senders. Tracks the peers subscribed
/// to different topics. The messages in the streams are wrapped in `Arc` as they technically could
/// be sent to multiple topics, but this api is not provided.
pub struct FloodsubStream {
    // Tracks the topic subscriptions.
    streams: HashMap<Topic, channel::UnboundedSender<Arc<FloodsubMessage>>>,
    // A collection of peers and the topics they are subscribed to.
    peers: HashMap<PeerId, Vec<Topic>>,
    floodsub: Floodsub,
    // the subscription streams implement Drop and will send out their topic name through the
    // sender cloned from here if they are dropped before the stream has ended.
    unsubscriptions: (
        channel::UnboundedSender<String>,
        channel::UnboundedReceiver<String>,
    ),
}

/// Stream of a pubsub messages. Implements [`FusedStream`].
pub struct SubscriptionStream {
    on_drop: Option<channel::UnboundedSender<String>>,
    topic: Option<String>,
    inner: channel::UnboundedReceiver<Arc<FloodsubMessage>>,
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        // the on_drop option allows us to disable this unsubscribe on drop once the stream has
        // ended.
        if let Some(sender) = self.on_drop.take() {
            if let Some(topic) = self.topic.take() {
                let _ = sender.unbounded_send(topic);
            }
        }
    }
}

impl fmt::Debug for SubscriptionStream {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if let Some(topic) = self.topic.as_ref() {
            write!(
                fmt,
                "SubscriptionStream {{ topic: {:?}, is_terminated: {} }}",
                topic,
                self.is_terminated()
            )
        } else {
            write!(
                fmt,
                "SubscriptionStream {{ is_terminated: {} }}",
                self.is_terminated()
            )
        }
    }
}

impl Stream for SubscriptionStream {
    type Item = Arc<FloodsubMessage>;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        use futures::stream::StreamExt;
        let inner = &mut self.as_mut().inner;
        match inner.poll_next_unpin(ctx) {
            Poll::Ready(None) => {
                // no need to unsubscribe on drop as the stream has already ended, likely via
                // unsubscribe call.
                self.on_drop.take();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl FusedStream for SubscriptionStream {
    fn is_terminated(&self) -> bool {
        self.on_drop.is_none()
    }
}

impl From<Floodsub> for FloodsubStream {
    fn from(floodsub: Floodsub) -> Self {
        let (tx, rx) = channel::unbounded();
        FloodsubStream {
            streams: HashMap::new(),
            peers: HashMap::new(),
            floodsub,
            unsubscriptions: (tx, rx),
        }
    }
}

impl FloodsubStream {
    /// Delegates the `peer_id` over to [`Floodsub::new`] and internally only does accounting on
    /// top of the floodsub.
    pub fn new(peer_id: PeerId) -> Self {
        let (tx, rx) = channel::unbounded();
        let mut config = FloodsubConfig::new(peer_id);
        config.subscribe_local_messages = true;
        FloodsubStream {
            streams: HashMap::new(),
            peers: HashMap::new(),
            floodsub: Floodsub::from_config(config),
            unsubscriptions: (tx, rx),
        }
    }

    /// Subscribes to a currently unsubscribed topic.
    /// Returns a receiver for messages sent to the topic or `None` if subscription existed
    /// already.
    pub fn subscribe(&mut self, topic: impl Into<String>) -> anyhow::Result<SubscriptionStream> {
        use std::collections::hash_map::Entry;

        let topic = Topic::new(topic);

        match self.streams.entry(topic) {
            Entry::Vacant(ve) => {
                // TODO: this could also be bounded; we could send the message and drop the
                // subscription if it ever became full.
                let (tx, rx) = channel::unbounded();

                // there are probably some invariants which need to hold for the topic...
                if !self.floodsub.subscribe(ve.key().clone()) {
                    anyhow::bail!("Unable to subscribe to topic");
                }

                let name = ve.key().id().to_string();
                ve.insert(tx);
                Ok(SubscriptionStream {
                    on_drop: Some(self.unsubscriptions.0.clone()),
                    topic: Some(name),
                    inner: rx,
                })
            }
            Entry::Occupied(_) => anyhow::bail!("Unable to subscribe to topic"),
        }
    }

    /// Unsubscribes from a topic. Unsubscription is usually done through dropping the
    /// SubscriptionStream.
    ///
    /// Returns true if an existing subscription was dropped, false otherwise
    pub fn unsubscribe(&mut self, topic: impl Into<String>) -> bool {
        let topic = Topic::new(topic);
        if self.streams.remove(&topic).is_some() {
            assert!(
                self.floodsub.unsubscribe(topic),
                "sender removed but unsubscription failed"
            );
            true
        } else {
            false
        }
    }

    /// See [`Floodsub::publish_many`]
    pub fn publish_many(
        &mut self,
        topic: impl IntoIterator<Item = impl Into<String>>,
        data: impl Into<Vec<u8>>,
    ) {
        let topic = topic.into_iter().map(Topic::new).collect::<Vec<_>>();
        self.floodsub.publish_many(topic, data);
    }

    /// See [`Floodsub::publish_any`]
    pub fn publish(&mut self, topic: impl Into<String>, data: impl Into<Vec<u8>>) {
        self.floodsub.publish(Topic::new(topic), data);
    }

    /// Returns the known peers subscribed to any topic
    pub fn known_peers(&self) -> Vec<PeerId> {
        self.peers.keys().cloned().collect()
    }

    /// Returns the peers known to subscribe to the given topic
    pub fn subscribed_peers(&self, topic: &Topic) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter_map(|(k, v)| if v.contains(topic) { Some(*k) } else { None })
            .collect()
    }

    /// Returns the list of currently subscribed topics. This can contain topics for which stream
    /// has been dropped but no messages have yet been received on the topics after the drop.
    pub fn subscribed_topics(&self) -> Vec<String> {
        self.streams
            .keys()
            .map(Topic::id)
            .map(String::from)
            .collect()
    }

    /// See [`Floodsub::add_node_from_partial_view`]
    pub fn add_node_to_partial_view(&mut self, peer_id: PeerId) {
        self.floodsub.add_node_to_partial_view(peer_id);
    }

    /// See [`Floodsub::remove_node_from_partial_view`]
    pub fn remove_node_from_partial_view(&mut self, peer_id: &PeerId) {
        self.floodsub.remove_node_from_partial_view(peer_id);
    }
}

impl NetworkBehaviour for FloodsubStream {
    type ConnectionHandler = <Floodsub as NetworkBehaviour>::ConnectionHandler;
    type OutEvent = FloodsubEvent;

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: libp2p::swarm::ConnectionId,
        peer: PeerId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        self.floodsub.handle_established_inbound_connection(
            connection_id,
            peer,
            local_addr,
            remote_addr,
        )
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: libp2p::swarm::ConnectionId,
        peer: PeerId,
        addr: &Multiaddr,
        role_override: libp2p::core::Endpoint,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        self.floodsub.handle_established_outbound_connection(
            connection_id,
            peer,
            addr,
            role_override,
        )
    }

    fn handle_pending_inbound_connection(
        &mut self,
        connection_id: libp2p::swarm::ConnectionId,
        local_addr: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<(), libp2p::swarm::ConnectionDenied> {
        self.floodsub
            .handle_pending_inbound_connection(connection_id, local_addr, remote_addr)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        connection_id: libp2p::swarm::ConnectionId,
        maybe_peer: Option<PeerId>,
        addresses: &[Multiaddr],
        effective_role: libp2p::core::Endpoint,
    ) -> Result<Vec<Multiaddr>, libp2p::swarm::ConnectionDenied> {
        self.floodsub.handle_pending_outbound_connection(
            connection_id,
            maybe_peer,
            addresses,
            effective_role,
        )
    }

    fn on_swarm_event(&mut self, event: libp2p::swarm::FromSwarm<Self::ConnectionHandler>) {
        self.floodsub.on_swarm_event(event)
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        connection_id: libp2p::swarm::ConnectionId,
        event: libp2p::swarm::THandlerOutEvent<Self>,
    ) {
        self.floodsub
            .on_connection_handler_event(peer_id, connection_id, event)
    }

    fn poll(
        &mut self,
        ctx: &mut Context,
        poll: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<libp2p::floodsub::FloodsubEvent, THandlerInEvent<Self>>> {
        use futures::stream::StreamExt;
        use std::collections::hash_map::Entry;

        loop {
            match self.unsubscriptions.1.poll_next_unpin(ctx) {
                Poll::Ready(Some(dropped)) => {
                    let topic = Topic::new(dropped);
                    if self.streams.remove(&topic).is_some() {
                        debug!("unsubscribing via drop from {:?}", topic.id());
                        assert!(
                            self.floodsub.unsubscribe(topic),
                            "Failed to unsubscribe a dropped subscription"
                        );
                    } else {
                        // unsubscribed already by `unsubscribe`
                        // TODO: not sure if the unsubscribe functionality is needed as the
                        // unsubscribe on drop seems to work
                    }
                }
                Poll::Ready(None) => unreachable!("we own the sender"),
                Poll::Pending => break,
            }
        }

        loop {
            match futures::ready!(self.floodsub.poll(ctx, poll)) {
                NetworkBehaviourAction::GenerateEvent(FloodsubEvent::Message(msg)) => {
                    let topics = msg.topics.clone();
                    let msg = Arc::new(msg);
                    let mut buffer = None;

                    for topic in topics {
                        if let Entry::Occupied(oe) = self.streams.entry(topic) {
                            let sent = buffer.take().unwrap_or_else(|| Arc::clone(&msg));

                            if let Err(se) = oe.get().unbounded_send(sent) {
                                // receiver has dropped
                                let (topic, _) = oe.remove_entry();
                                debug!("unsubscribing via SendError from {:?}", topic.id());
                                assert!(
                                    self.floodsub.unsubscribe(topic),
                                    "Failed to unsubscribe following SendError"
                                );
                                buffer = Some(se.into_inner());
                            }
                        } else {
                            // we had unsubscribed from the topic after Floodsub had received the
                            // message
                        }
                    }

                    continue;
                }
                NetworkBehaviourAction::GenerateEvent(FloodsubEvent::Subscribed {
                    peer_id,
                    topic,
                }) => {
                    self.add_node_to_partial_view(peer_id);
                    let topics = self.peers.entry(peer_id).or_insert_with(Vec::new);

                    let appeared = topics.is_empty();

                    if !topics.contains(&topic) {
                        topics.push(topic);
                    }

                    if appeared {
                        debug!("peer appeared as pubsub subscriber: {}", peer_id);
                    }

                    continue;
                }
                NetworkBehaviourAction::GenerateEvent(FloodsubEvent::Unsubscribed {
                    peer_id,
                    topic,
                }) => {
                    if let Entry::Occupied(mut oe) = self.peers.entry(peer_id) {
                        let topics = oe.get_mut();
                        if let Some(pos) = topics.iter().position(|t| t == &topic) {
                            topics.swap_remove(pos);
                        }
                        if topics.is_empty() {
                            debug!("peer disappeared as pubsub subscriber: {}", peer_id);
                            oe.remove();
                            self.remove_node_from_partial_view(&peer_id);
                        }
                    }

                    continue;
                }
                action @ NetworkBehaviourAction::Dial { .. } => {
                    return Poll::Ready(action);
                }
                NetworkBehaviourAction::NotifyHandler {
                    peer_id,
                    event,
                    handler,
                } => {
                    return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id,
                        event,
                        handler,
                    });
                }
                NetworkBehaviourAction::ReportObservedAddr { address, score } => {
                    return Poll::Ready(NetworkBehaviourAction::ReportObservedAddr {
                        address,
                        score,
                    });
                }
                NetworkBehaviourAction::CloseConnection {
                    peer_id,
                    connection,
                } => {
                    return Poll::Ready(NetworkBehaviourAction::CloseConnection {
                        peer_id,
                        connection,
                    });
                }
            }
        }
    }
}
