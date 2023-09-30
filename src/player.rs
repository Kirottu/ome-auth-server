use crate::{html, manager, ome_statistics, Config, Html, StreamConfig};
use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{
    get,
    web::{Data, Payload, Query},
    HttpRequest, HttpResponse, Result,
};
use actix_web_actors::ws;
use futures::StreamExt;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::time::{Duration, Instant};
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
    const INTERVAL: Duration = Duration::from_secs(1);
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

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

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
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
        self.manager.do_send(manager::Enqueue {
            uuid: self.uuid,
            ip_addr: self.ip_addr.clone(),
            addr: ctx.address(),
            stream: self.stream.clone(),
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        self.manager.do_send(manager::Dequeue {
            uuid: self.uuid,
            stream: self.stream.clone(),
        });
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
            ctx.text(html! {"../res/player/denied.html"});
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
                ctx.stop();
            }
        }
    }
}

#[get("/player")]
async fn index(config: Data<Config>, pool: Data<MySqlPool>, agent: Data<ureq::Agent>) -> Html {
    let streams = sqlx::query_as!(StreamConfig, "SELECT * FROM streams")
        .fetch_all(&*pool.into_inner())
        .await
        .unwrap();

    let buttons = futures::stream::iter(streams.iter().map(|stream| stream.id.clone()))
        .filter_map(|stream| {
            let config = config.clone();
            let agent = agent.clone();
            async move {
                ome_statistics(config, agent, &stream)
                    .ok()
                    .map(|_| html! {"../res/player/available_stream.html", "{stream}" => stream})
            }
        })
        .collect::<Vec<String>>()
        .await;

    let content = if buttons.is_empty() {
        "<p><code>No streams available.</code></p>".to_string()
    } else {
        format!(
            "<p><code>Select a stream to watch.</code></p> {}",
            buttons.join("")
        )
    };

    Html(html! {"../res/player/index.html",
        "{content}" => content
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
    config: Data<Config>,
    req: HttpRequest,
    payload: Payload,
    query: Query<StreamQuery>,
) -> Result<HttpResponse> {
    let addr = req
        .headers()
        .get("X-Real-IP")
        .map(|value| value.to_str().unwrap().to_string())
        .unwrap_or(
            req.peer_addr()
                .map(|addr| addr.ip().to_string())
                .unwrap_or("N/A".to_string()),
        );

    ws::start(
        QueueWebSocket::new(
            manager.get_ref().clone(),
            config.ome_host.clone(),
            addr,
            query.stream.clone(),
        ),
        &req,
        payload,
    )
}
