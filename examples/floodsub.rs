use futures::StreamExt;
use libp2p::swarm::NetworkBehaviour;
use libp2p::tcp::Config as GenTcpConfig;
use libp2p::Swarm;
use libp2p::{
    core::upgrade,
    floodsub::FloodsubEvent,
    identity,
    mdns::{tokio::Behaviour as Mdns, Event as MdnsEvent},
    mplex, noise,
    swarm::SwarmEvent,
    tcp::tokio::Transport as TokioTcpTransport,
    Multiaddr, PeerId, Transport,
};
use libp2p_helper::floodsub::FloodsubStream;
use std::error::Error;
use tokio::io::{self, AsyncBufReadExt};

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "MyBehaviourEvent")]
struct MyBehaviour {
    floodsub: FloodsubStream,
    mdns: Mdns,
}

enum MyBehaviourEvent {
    Floodsub(FloodsubEvent),
    Mdns(MdnsEvent),
}

impl From<FloodsubEvent> for MyBehaviourEvent {
    fn from(event: FloodsubEvent) -> Self {
        MyBehaviourEvent::Floodsub(event)
    }
}

impl From<MdnsEvent> for MyBehaviourEvent {
    fn from(event: MdnsEvent) -> Self {
        MyBehaviourEvent::Mdns(event)
    }
}

/// The `tokio::main` attribute sets up a tokio runtime.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Create a random PeerId
    let id_keys = identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(id_keys.public());
    println!("Local peer id: {:?}", peer_id);

    // Create a keypair for authenticated encryption of the transport.
    let noise_keys = noise::Keypair::<noise::X25519Spec>::new()
        .into_authentic(&id_keys)
        .expect("Signing libp2p-noise static DH keypair failed.");

    // Create a tokio-based TCP transport use noise for authenticated
    // encryption and Mplex for multiplexing of substreams on a TCP stream.
    let transport = TokioTcpTransport::new(GenTcpConfig::default().nodelay(true))
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::NoiseConfig::xx(noise_keys).into_authenticated())
        .multiplex(mplex::MplexConfig::new())
        .boxed();

    // Create a Floodsub topic
    let topic = "chat";

    // We create a custom network behaviour that combines floodsub and mDNS.
    // The derive generates a delegating `NetworkBehaviour` impl which in turn
    // requires the implementations of `NetworkBehaviourEventProcess` for
    // the events of each behaviour.

    // Create a Swarm to manage peers and events.
    let mut swarm = {
        let mdns = Mdns::new(Default::default(), peer_id)?;
        let behaviour = MyBehaviour {
            floodsub: FloodsubStream::new(peer_id),
            mdns,
        };

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
    let stream = swarm.behaviour_mut().floodsub.subscribe(topic).unwrap();

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
                swarm.behaviour_mut().floodsub.publish(topic, line.as_bytes());
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
                                    swarm.behaviour_mut().floodsub.add_node_to_partial_view(peer);
                                }
                            }
                            MdnsEvent::Expired(list) => {
                                for (peer, _) in list {
                                    if !swarm.behaviour_mut().mdns.has_node(&peer) {
                                        swarm.behaviour_mut().floodsub.remove_node_from_partial_view(&peer);
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
