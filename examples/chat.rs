// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! A basic chat application with logs demonstrating libp2p and the gossipsub protocol
//! combined with mDNS for the discovery of peers to gossip with.
//!
//! Using two terminal windows, start two instances, typing the following in each:
//!
//! ```sh
//! cargo run
//! ```
//!
//! Mutual mDNS discovery may take a few seconds. When each peer does discover the other
//! it will print a message like:
//!
//! ```sh
//! mDNS discovered a new peer: {peerId}
//! ```
//!
//! Type a message and hit return: the message is sent and printed in the other terminal.
//! Close with Ctrl-c.
//!
//! You can open more terminal windows and add more peers using the same line above.
//!
//! Once an additional peer is mDNS discovered it can participate in the conversation
//! and all peers will receive messages sent from it.
//!
//! If a participant exits (Control-C or otherwise) the other peers will receive an mDNS expired
//! event and remove the expired peer from the list of known peers.

use futures::{prelude::*, select};
use libp2p::{
    core::muxing::StreamMuxerBox,
    gossipsub, identity, mdns,
    swarm::NetworkBehaviour,
    swarm::{SwarmBuilder, SwarmEvent},
    PeerId, Transport,
};
use rust_libp2p_nym::{test_utils::create_nym_client, transport::NymTransport};
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use testcontainers::clients;
use tokio::io;
use tokio_util::codec;
use tracing::info;
use tracing_subscriber::EnvFilter;

// We create a custom network behaviour that combines Gossipsub and Mdns.
#[derive(NetworkBehaviour)]
struct Behaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    // Create a random PeerId
    let id_keys = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(id_keys.public());
    println!("Local peer id: {local_peer_id}");

    // To content-address message, we can take the hash of message and use it as an ID.
    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };

    // Set a custom gossipsub configuration
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
        .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
        .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
        .build()
        .expect("Valid config");

    // build a gossipsub network behaviour
    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(id_keys),
        gossipsub_config,
    )
    .expect("Correct configuration");
    // Create a Gossipsub topic
    let topic = gossipsub::IdentTopic::new("test-net");
    // subscribes to our topic
    gossipsub.subscribe(&topic)?;

    let nym_id = rand::random::<u64>().to_string();
    let docker_client = clients::Cli::default();
    let (_nym_container, nym_port, dialer_uri) = create_nym_client(&docker_client, &nym_id);
    info!("nym_port: {}", nym_port);
    info!("dialer_uri: {}", dialer_uri);

    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    info!("Local peer id: {local_peer_id:?}");

    let transport = NymTransport::new(&dialer_uri, local_key).await?;
    let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;

    let mut swarm = SwarmBuilder::with_tokio_executor(
        transport
            .map(|a, _| (a.0, StreamMuxerBox::new(a.1)))
            .boxed(),
        Behaviour { gossipsub, mdns },
        local_peer_id,
    )
    .build();

    println!("swarm has been built");

    // Read full lines from stdin
    let mut stdin = codec::FramedRead::new(io::stdin(), codec::LinesCodec::new()).fuse();

    //swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    println!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    // Kick it off
    loop {
        select! {
            line = stdin.select_next_some() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.expect("Stdin not to close").as_bytes()) {
                    println!("Publish error: {e:?}");
                }
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discovered a new peer: {peer_id}");
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discover peer has expired: {peer_id}");
                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => println!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        String::from_utf8_lossy(&message.data),
                    ),
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local node is listening on {address}");
                }
                _ => {}
            }
        }
    }
}
