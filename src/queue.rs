use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use actix::{
    Actor, ActorContext, Addr, AsyncContext, Context, Handler, Message, Recipient, StreamHandler,
};
use actix_web_actors::ws;

use crate::{Client, QueuedRequest};

pub struct QueueWebSocket {
    hb: Instant,
    addr: Addr<QueueActor>,
    stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Refresh(Vec<(Client, QueuedRequest)>);

#[derive(Message)]
#[rtype(result = "()")]
pub struct Handle {
    pub client: Client,
    pub allow: bool,
    pub stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct NewConnection {
    pub client: Client,
    pub request: QueuedRequest,
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
    queue: HashMap<Client, QueuedRequest>,
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
                self.addr.do_send(RecipientDisconnected {
                    recipient: ctx.address().recipient(),
                    stream: act.stream.clone(),
                });
                ctx.stop();
            } else {
                ctx.ping(&[]);
            }
        });
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
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for QueueWebSocket {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match item {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(_)) => self.hb = Instant::now(),
            _ => ctx.stop(),
        }
    }
}

impl Handler<Refresh> for QueueWebSocket {
    type Result = <Refresh as Message>::Result;

    fn handle(&mut self, msg: Refresh, ctx: &mut Self::Context) -> Self::Result {}
}

pub struct QueueActor {
    streams: HashMap<String, StreamData>,
}

impl QueueActor {
    pub fn new(_streams: &[String]) -> Self {
        Self {
            streams: _streams
                .into_iter()
                .map(|stream| (*stream, StreamData::default()))
                .collect(),
        }
    }

    fn refresh_sockets(&self, stream: &str) {
        let mut queue = self.streams[stream]
            .queue
            .clone()
            .into_iter()
            .map(|val| val)
            .collect::<Vec<_>>();

        queue.sort_by(|a, b| a.1.instant.elapsed().cmp(&b.1.instant.elapsed()));

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

    fn handle(&mut self, msg: Handle, ctx: &mut Self::Context) -> Self::Result {
        if let Some(request) = self.streams[&msg.stream].queue.remove(&msg.client) {
            request.sender.send(msg.allow).unwrap();
            self.refresh_sockets(&msg.stream);
        }
    }
}

impl Handler<NewConnection> for QueueActor {
    type Result = <NewConnection as Message>::Result;

    fn handle(&mut self, msg: NewConnection, ctx: &mut Self::Context) -> Self::Result {
        todo!()
    }
}

impl Handler<NewRecipient> for QueueActor {
    type Result = <NewRecipient as Message>::Result;

    fn handle(&mut self, msg: NewRecipient, ctx: &mut Self::Context) -> Self::Result {
        self.streams[&msg.stream]
            .queue_recipients
            .push(msg.recipient);
    }
}

impl Handler<RecipientDisconnected> for QueueActor {
    type Result = <RecipientDisconnected as Message>::Result;

    fn handle(&mut self, msg: RecipientDisconnected, ctx: &mut Self::Context) -> Self::Result {
        self.streams[&msg.stream]
            .queue_recipients
            .retain(|recipient| *recipient != msg.recipient);
    }
}
