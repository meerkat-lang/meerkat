//! actor system and pub sub system
//! - choice 1: implement actor system using tokio::sync::mpsc ourself
//!             below is a premature implementation
//! - choice 2: use https://github.com/tqwewe/kameo (for now)

use kameo::actor::ActorRef;

use super::{def_actor::DefActor, message::Msg};

/// - (layer 1: actor system) communicate messages between local / remote nodes
///    use kameo
/// - (layer 2: pub/subscribers) maintain network topology between nodes
///    similar to kameo/actors/src/pubsub.rs
pub struct PubSub {
    subscribers: Vec<ActorRef<DefActor>>, // todo: generalize to all actors
}

impl PubSub {
    pub fn new() -> Self {
        PubSub {
            subscribers: Vec::new(),
        }
    }

    pub fn subscribe(&mut self, subscriber: ActorRef<DefActor>) {
        self.subscribers.push(subscriber);
    }

    /// developer note: don't use future.join_all() overhead there
    /// https://github.com/tqwewe/kameo/issues/157
    pub async fn publish(&self, msg: Msg) {
        for subscriber in &self.subscribers {
            if let Err(e) = subscriber.tell(msg.clone()).await {
                eprintln!(
                    "Failed to send message to subscriber {:?}: {:?}",
                    subscriber, e
                );
            }
        }
    }
}

// todo:
// 1. make pubsub more generic, as a template
// 2. more functionalites
// 3. more fault tolerance
// 4. models other than direct publish to subscribers
