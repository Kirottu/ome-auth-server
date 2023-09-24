use std::collections::HashMap;

use actix::{Actor, Addr, Context, Handler, Message, Recipient};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{dashboard, player, Client};

#[derive(Message, Serialize, Deserialize)]
#[rtype(result = "()")]
pub struct Handle {
    pub uuid: Uuid,
    pub allow: bool,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Enqueue {
    pub uuid: Uuid,
    pub ip_addr: String,
    pub addr: Addr<player::QueueWebSocket>,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Dequeue {
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

#[derive(Default)]
struct StreamData {
    queue: HashMap<Uuid, QueuedPlayer>,
    queue_recipients: Vec<Recipient<dashboard::Refresh>>,
}

#[derive(Clone)]
pub struct QueuedPlayer {
    pub ip_addr: String,
    addr: Addr<player::QueueWebSocket>,
    allow: bool,
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
        if let Some(queued_player) = self
            .streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue
            .get_mut(&msg.uuid)
        {
            queued_player.allow = msg.allow;
            queued_player.addr.do_send(player::Handle(msg.allow));
        }
    }
}

impl Handler<Enqueue> for Manager {
    type Result = <Enqueue as Message>::Result;

    fn handle(&mut self, msg: Enqueue, _ctx: &mut Self::Context) -> Self::Result {
        self.streams.get_mut(&msg.stream).unwrap().queue.insert(
            msg.uuid,
            QueuedPlayer {
                ip_addr: msg.ip_addr,
                addr: msg.addr,
                allow: false,
            },
        );
        self.refresh_sockets(Some(msg.uuid), &msg.stream);
    }
}

impl Handler<Dequeue> for Manager {
    type Result = <Dequeue as Message>::Result;

    fn handle(&mut self, msg: Dequeue, _ctx: &mut Self::Context) -> Self::Result {
        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue
            .remove(&msg.uuid);
        self.refresh_sockets(None, &msg.stream);
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
        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue_recipients
            .retain(|recipient| *recipient != msg.recipient);
    }
}

impl Handler<IsAuthorized> for Manager {
    type Result = <IsAuthorized as Message>::Result;

    fn handle(&mut self, msg: IsAuthorized, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(queued_player) = self
            .streams
            .get_mut(&msg.stream)
            .and_then(|stream| stream.queue.remove(&msg.uuid))
        {
            queued_player.addr.do_send(player::Stop);
            queued_player.allow
        } else {
            tracing::info!("Uuid missing, denied access");
            false
        }
    }
}