use futures::stream::StreamExt;
use libp2p::swarm::NetworkBehaviour;
use libp2p::tcp::Config as GenTcpConfig;
use libp2p::Swarm;
use libp2p::{
    core::upgrade,
    gossipsub::Event as GossipsubEvent,
    identity,
    mdns::{tokio::Behaviour as Mdns, Event as MdnsEvent},
    mplex, noise,
    swarm::{SwarmEvent},
    tcp::tokio::Transport as TokioTcpTransport,
    Multiaddr, PeerId, Transport,
};
use libp2p_helper::gossipsub::GossipsubStream;
use tokio::io::{self, AsyncBufReadExt};

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "MyBehaviourEvent")]
struct MyBehaviour {
    gossipsub: GossipsubStream,
    mdns: Mdns,
}

enum MyBehaviourEvent {
    Gossipsub(GossipsubEvent),
    Mdns(MdnsEvent),
}

impl From<GossipsubEvent> for MyBehaviourEvent {
    fn from(event: GossipsubEvent) -> Self {
        MyBehaviourEvent::Gossipsub(event)
    }
}

impl From<MdnsEvent> for MyBehaviourEvent {
    fn from(event: MdnsEvent) -> Self {
        MyBehaviourEvent::Mdns(event)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a random PeerId
    let id_keys = identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(id_keys.public());
    println!("Local peer id: {:?}", peer_id);

    // Create a keypair for authenticated encryption of the transport.
    let noise_keys = noise::Keypair::<noise::X25519Spec>::new().into_authentic(&id_keys)?;

    // Create a tokio-based TCP transport use noise for authenticated
    // encryption and Mplex for multiplexing of substreams on a TCP stream.
    let transport = TokioTcpTransport::new(GenTcpConfig::default().nodelay(true))
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
        .multiplex(mplex::MplexConfig::new())
        .boxed();

    let topic = "chat";

    // Create a Swarm to manage peers and events.
    let mut swarm = {
        //You can also do GossipsubStream::from to import gossipsub
        let gossipsub = libp2p_helper::gossipsub::GossipsubStream::new(id_keys)?;

        let mdns = Mdns::new(Default::default(), peer_id)?;
        let behaviour = MyBehaviour { gossipsub, mdns };

        Swarm::with_tokio_executor(transport, behaviour, peer_id)
    };

    // Reach out to another node if specified
    if let Some(to_dial) = std::env::args().nth(1) {
        let addr: Multiaddr = to_dial.parse()?;
        swarm.dial(addr)?;
        println!("Dialed {:?}", to_dial);
    }

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    // Listen on all interfaces and whatever port the OS assigns
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // Subscribe to topic
    let stream = swarm.behaviour_mut().gossipsub.subscribe(topic).unwrap();

    // pin stream
    futures::pin_mut!(stream);

    // Kick it off
    loop {
        tokio::select! {
            msg = stream.next() => {
                if let Some(msg) = msg {
                    println!("{}", String::from_utf8_lossy(&msg.data));
                }
            }
            line = stdin.next_line() => {
                let line = line?.expect("stdin closed");
                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic, line.as_bytes()) {
                    println!("Error publishing message: {}", e);
                }
            }
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        println!("Listening on {:?}", address);
                    },
                    SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(event)) => {
                        match event {
                            MdnsEvent::Discovered(list) => {
                                for (peer, _) in list {
                                    swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                                }
                            }
                            MdnsEvent::Expired(list) => {
                                for (peer, _) in list {
                                    if !swarm.behaviour_mut().mdns.has_node(&peer) {
                                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
