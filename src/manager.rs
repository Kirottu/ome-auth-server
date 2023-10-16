use std::collections::{HashMap, HashSet};

use actix::{Actor, Addr, Context, Handler, Message, Recipient};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{dashboard, player};

#[derive(Message, Serialize, Deserialize)]
#[rtype(result = "()")]
pub struct Handle {
    pub uuid: Uuid,
    pub allow: bool,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct PlayerConnect {
    pub uuid: Uuid,
    pub ip_addr: String,
    pub addr: Addr<player::QueueWebSocket>,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct PlayerDisconnect {
    pub uuid: Uuid,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "bool")]
pub struct IsAuthorized {
    pub uuid: Uuid,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct NewRecipient {
    pub stream: String,
    pub recipient: Recipient<dashboard::Refresh>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct RecipientDisconnected {
    pub stream: String,
    pub recipient: Recipient<dashboard::Refresh>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct StreamCreated {
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct StreamRemoved {
    pub stream: String,
}

#[derive(Default, Debug)]
struct StreamData {
    queue: HashMap<Uuid, QueuedPlayer>,
    authorized_players: HashSet<Uuid>,
    queue_recipients: Vec<Recipient<dashboard::Refresh>>,
}

#[derive(Clone, Debug)]
pub struct QueuedPlayer {
    pub ip_addr: String,
    addr: Addr<player::QueueWebSocket>,
}

pub struct Manager {
    streams: HashMap<String, StreamData>,
}

impl Manager {
    pub fn new(_streams: &[String]) -> Self {
        Self {
            streams: _streams
                .iter()
                .map(|stream| (stream.clone(), StreamData::default()))
                .collect(),
        }
    }

    fn refresh_sockets(&self, new: Option<Uuid>, stream: &str) {
        let queue = self.streams[stream]
            .queue
            .clone()
            .into_iter()
            .collect::<Vec<_>>();

        for recipient in &self.streams[stream].queue_recipients {
            recipient.do_send(dashboard::Refresh {
                queue: queue.clone(),
                new,
            });
        }
    }
}

impl Actor for Manager {
    type Context = Context<Self>;
}

impl Handler<Handle> for Manager {
    type Result = <Handle as Message>::Result;

    fn handle(&mut self, msg: Handle, _ctx: &mut Self::Context) -> Self::Result {
        let stream = self.streams.get_mut(&msg.stream).unwrap();
        if let Some(queued_player) = stream.queue.remove(&msg.uuid) {
            // If allowed, add it to the list of authorized clients
            if msg.allow {
                stream.authorized_players.insert(msg.uuid);
            }

            queued_player.addr.do_send(player::Handle(msg.allow));
            self.refresh_sockets(None, &msg.stream);
        }
    }
}

impl Handler<PlayerConnect> for Manager {
    type Result = <PlayerConnect as Message>::Result;

    fn handle(&mut self, msg: PlayerConnect, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(stream) = self.streams.get_mut(&msg.stream) {
            stream.queue.insert(
                msg.uuid,
                QueuedPlayer {
                    ip_addr: msg.ip_addr,
                    addr: msg.addr,
                },
            );
            self.refresh_sockets(Some(msg.uuid), &msg.stream);
        }
    }
}

impl Handler<PlayerDisconnect> for Manager {
    type Result = <PlayerDisconnect as Message>::Result;

    fn handle(&mut self, msg: PlayerDisconnect, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(stream) = self.streams.get_mut(&msg.stream) {
            // Make sure neither of the lists contain the player
            stream.queue.remove(&msg.uuid);
            stream.authorized_players.remove(&msg.uuid);
            self.refresh_sockets(None, &msg.stream);
        }
    }
}

impl Handler<NewRecipient> for Manager {
    type Result = <NewRecipient as Message>::Result;

    fn handle(&mut self, msg: NewRecipient, _ctx: &mut Self::Context) -> Self::Result {
        msg.recipient.do_send(dashboard::Refresh {
            queue: self.streams[&msg.stream]
                .queue
                .clone()
                .into_iter()
                .collect(),
            new: None,
        });

        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue_recipients
            .push(msg.recipient);
    }
}

impl Handler<RecipientDisconnected> for Manager {
    type Result = <RecipientDisconnected as Message>::Result;

    fn handle(&mut self, msg: RecipientDisconnected, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(stream) = self.streams.get_mut(&msg.stream) {
            stream
                .queue_recipients
                .retain(|recipient| *recipient != msg.recipient);
        }
    }
}

impl Handler<IsAuthorized> for Manager {
    type Result = <IsAuthorized as Message>::Result;

    fn handle(&mut self, msg: IsAuthorized, _ctx: &mut Self::Context) -> Self::Result {
        if self
            .streams
            .get_mut(&msg.stream)
            .and_then(|stream| stream.authorized_players.get(&msg.uuid))
            .is_some()
        {
            true
        } else {
            tracing::info!("Uuid missing, denied access");
            false
        }
    }
}

impl Handler<StreamCreated> for Manager {
    type Result = <StreamCreated as Message>::Result;

    fn handle(&mut self, msg: StreamCreated, _ctx: &mut Self::Context) -> Self::Result {
        self.streams.insert(msg.stream, StreamData::default());
    }
}

impl Handler<StreamRemoved> for Manager {
    type Result = <StreamRemoved as Message>::Result;

    fn handle(&mut self, msg: StreamRemoved, _ctx: &mut Self::Context) -> Self::Result {
        self.streams.remove(&msg.stream);
    }
}
