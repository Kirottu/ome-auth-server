use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use actix::{
    Actor, ActorContext, Addr, AsyncContext, Context, Handler, Message, Recipient, StreamHandler,
};
use actix_web_actors::ws;
use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};

use crate::{html, Client};

pub struct QueueWebSocket {
    hb: Instant,
    addr: Addr<QueueActor>,
    stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Refresh(Vec<Client>);

#[derive(Message, Serialize, Deserialize)]
#[rtype(result = "bool")]
pub struct Handle {
    pub client: Client,
    pub allow: bool,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Enqueue {
    pub client: Client,
    pub sender: UnboundedSender<bool>,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Dequeue {
    pub client: Client,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct NewRecipient {
    pub stream: String,
    pub recipient: Recipient<Refresh>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct RecipientDisconnected {
    pub stream: String,
    pub recipient: Recipient<Refresh>,
}

#[derive(Default)]
struct StreamData {
    queue: HashMap<Client, UnboundedSender<bool>>,
    queue_recipients: Vec<Recipient<Refresh>>,
}

impl QueueWebSocket {
    const INTERVAL: Duration = Duration::from_secs(1);
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(addr: Addr<QueueActor>, stream: String) -> Self {
        Self {
            addr,
            stream,
            hb: Instant::now(),
        }
    }

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
                act.addr.do_send(RecipientDisconnected {
                    recipient: ctx.address().recipient(),
                    stream: act.stream.clone(),
                });
                ctx.stop();
            } else {
                ctx.ping(&[]);
            }
        });
    }

    fn refresh(&self, queue: &[Client], ctx: &mut ws::WebsocketContext<Self>) {
        let content = queue
            .iter()
            .map(|client| {
                html! {"../res/queued_connection.html",
                    "{ip}" => client.address,
                    "{port}" => client.port,
                    "{allow}" => serde_json::to_string(&Handle {client: client.clone(), allow: true, stream: self.stream.clone()}).unwrap(),
                    "{reject}" => serde_json::to_string(&Handle {client: client.clone(), allow: false, stream: self.stream.clone()}).unwrap()
                }
            })
            .collect::<String>();

        ctx.text(html! {"../res/queue.html", "{content}" => &content})
    }
}

impl Actor for QueueWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);

        self.addr.do_send(NewRecipient {
            stream: self.stream.clone(),
            recipient: ctx.address().recipient(),
        });
        self.refresh(&[], ctx);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for QueueWebSocket {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match item {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Text(text)) => {
                self.addr
                    .do_send(serde_json::from_str::<Handle>(&text).unwrap());
            }
            Ok(ws::Message::Pong(_)) => self.hb = Instant::now(),
            _ => ctx.stop(),
        }
    }
}

impl Handler<Refresh> for QueueWebSocket {
    type Result = <Refresh as Message>::Result;

    fn handle(&mut self, msg: Refresh, ctx: &mut Self::Context) -> Self::Result {
        self.refresh(&msg.0, ctx);
    }
}

pub struct QueueActor {
    streams: HashMap<String, StreamData>,
}

impl QueueActor {
    pub fn new(_streams: &[String]) -> Self {
        Self {
            streams: _streams
                .iter()
                .map(|stream| (stream.clone(), StreamData::default()))
                .collect(),
        }
    }

    fn refresh_sockets(&self, stream: &str) {
        let queue = self.streams[stream]
            .queue
            .clone()
            .into_keys()
            .collect::<Vec<_>>();

        for recipient in &self.streams[stream].queue_recipients {
            recipient.do_send(Refresh(queue.clone()));
        }
    }
}

impl Actor for QueueActor {
    type Context = Context<Self>;
}

impl Handler<Handle> for QueueActor {
    type Result = <Handle as Message>::Result;

    fn handle(&mut self, msg: Handle, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(sender) = self
            .streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue
            .remove(&msg.client)
        {
            sender.unbounded_send(msg.allow).unwrap();
            self.refresh_sockets(&msg.stream);
            true
        } else {
            false
        }
    }
}

impl Handler<Enqueue> for QueueActor {
    type Result = <Enqueue as Message>::Result;

    fn handle(&mut self, msg: Enqueue, _ctx: &mut Self::Context) -> Self::Result {
        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue
            .insert(msg.client, msg.sender);
        self.refresh_sockets(&msg.stream);
    }
}

impl Handler<Dequeue> for QueueActor {
    type Result = <Dequeue as Message>::Result;

    fn handle(&mut self, msg: Dequeue, _ctx: &mut Self::Context) -> Self::Result {
        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue
            .remove(&msg.client);
        self.refresh_sockets(&msg.stream);
    }
}

impl Handler<NewRecipient> for QueueActor {
    type Result = <NewRecipient as Message>::Result;

    fn handle(&mut self, msg: NewRecipient, _ctx: &mut Self::Context) -> Self::Result {
        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue_recipients
            .push(msg.recipient);
    }
}

impl Handler<RecipientDisconnected> for QueueActor {
    type Result = <RecipientDisconnected as Message>::Result;

    fn handle(&mut self, msg: RecipientDisconnected, _ctx: &mut Self::Context) -> Self::Result {
        self.streams
            .get_mut(&msg.stream)
            .unwrap()
            .queue_recipients
            .retain(|recipient| *recipient != msg.recipient);
    }
}
