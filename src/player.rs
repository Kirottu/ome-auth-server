use crate::{html, manager, ome_statistics, Html, State};
use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{
    get,
    web::{Data, Payload, Query},
    HttpRequest, HttpResponse, Result,
};
use actix_web_actors::ws;
use futures::StreamExt;
use serde::Deserialize;
use std::time::Instant;
use uuid::Uuid;

pub struct QueueWebSocket {
    hb: Instant,
    uuid: Uuid,
    manager: Addr<manager::Manager>,
    ip_addr: String,
    ome_host: String,
    stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Handle(pub bool);

#[derive(Message)]
#[rtype(result = "()")]
pub struct Stop;

#[derive(Deserialize)]
struct StreamQuery {
    stream: String,
}

impl QueueWebSocket {
    fn new(
        manager: Addr<manager::Manager>,
        ome_host: String,
        ip_addr: String,
        stream: String,
    ) -> Self {
        Self {
            hb: Instant::now(),
            uuid: Uuid::new_v4(),
            manager,
            ip_addr,
            ome_host,
            stream,
        }
    }
}

impl Actor for QueueWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.manager.do_send(manager::Enqueue {
            uuid: self.uuid,
            ip_addr: self.ip_addr.clone(),
            addr: ctx.address(),
            stream: self.stream.clone(),
        })
    }
}

impl Handler<Handle> for QueueWebSocket {
    type Result = <Handle as Message>::Result;

    fn handle(&mut self, msg: Handle, ctx: &mut Self::Context) -> Self::Result {
        if msg.0 {
            ctx.text(html! {"../res/player/player.html",
                "{host}" => self.ome_host,
                "{uuid}" => self.uuid,
                "{stream}" => self.stream
            });
        } else {
            ctx.text(html! {"../res/player/rejected.html"});
            self.manager.do_send(manager::Dequeue {
                uuid: self.uuid,
                stream: self.stream.clone(),
            });
            ctx.close(None);
        }
    }
}

impl Handler<Stop> for QueueWebSocket {
    type Result = <Stop as Message>::Result;

    fn handle(&mut self, _msg: Stop, ctx: &mut Self::Context) -> Self::Result {
        ctx.close(None);
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
            _ => {
                self.manager.do_send(manager::Dequeue {
                    uuid: self.uuid,
                    stream: self.stream.clone(),
                });
                ctx.stop();
            }
        }
    }
}

#[get("/player")]
async fn player(state: Data<State>) -> Html {
    let buttons = futures::stream::iter(state.config.api_keys.values())
        .filter_map(|stream| async {
            ome_statistics(state.clone(), stream)
                .ok()
                .map(|_| html! {"../res/player/available-stream.html", "{stream}" => stream})
        })
        .collect::<String>()
        .await;
    Html(html! {"../res/player/selector.html",
        "{buttons}" => buttons
    })
}

#[get("/player/enqueue")]
async fn enqueue(query: Query<StreamQuery>) -> Html {
    Html(html! {"../res/player/enqueue.html",
        "{stream}" => query.stream
    })
}

#[get("/player/queue_ws")]
async fn queue_ws(
    manager: Data<Addr<manager::Manager>>,
    state: Data<State>,
    req: HttpRequest,
    payload: Payload,
    query: Query<StreamQuery>,
) -> Result<HttpResponse> {
    ws::start(
        QueueWebSocket::new(
            manager.get_ref().clone(),
            state.config.ome_host.clone(),
            req.peer_addr()
                .map(|addr| addr.ip().to_string())
                .unwrap_or("N/A".to_string()),
            query.stream.clone(),
        ),
        &req,
        payload,
    )
}
