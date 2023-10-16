use crate::{manager, ome_statistics, Config, Html, StreamConfig};
use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{
    get,
    web::{Data, Payload, Query},
    HttpRequest, HttpResponse, Result,
};
use actix_web_actors::ws;
use askama::Template;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod templates {
    use askama::Template;
    use uuid::Uuid;

    #[derive(Template)]
    #[template(path = "player/index.html")]
    pub struct Index<'a> {
        pub streams: &'a [&'a String],
    }

    #[derive(Template)]
    #[template(path = "player/enqueue.html")]
    pub struct Enqueue<'a> {
        pub stream: &'a str,
    }

    #[derive(Template)]
    #[template(path = "player/player.html")]
    pub struct Player<'a> {
        pub stream: &'a str,
        pub host: &'a str,
        pub uuid: &'a Uuid,
    }

    #[derive(Template)]
    #[template(path = "player/denied.html")]
    pub struct Denied;
}

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
        self.manager.do_send(manager::PlayerConnect {
            uuid: self.uuid,
            ip_addr: self.ip_addr.clone(),
            addr: ctx.address(),
            stream: self.stream.clone(),
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        self.manager.do_send(manager::PlayerDisconnect {
            uuid: self.uuid,
            stream: self.stream.clone(),
        });
    }
}

impl Handler<Handle> for QueueWebSocket {
    type Result = <Handle as Message>::Result;

    fn handle(&mut self, msg: Handle, ctx: &mut Self::Context) -> Self::Result {
        if msg.0 {
            ctx.text(
                templates::Player {
                    stream: &self.stream,
                    host: &self.ome_host,
                    uuid: &self.uuid,
                }
                .render()
                .unwrap(),
            );
        } else {
            ctx.text(templates::Denied.render().unwrap());
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

    let streams = streams
        .iter()
        .filter_map(|stream| {
            ome_statistics(config.clone(), agent.clone(), &stream.id)
                .ok()
                .map(move |_| &stream.id)
        })
        .collect::<Vec<_>>();

    Html(templates::Index { streams: &streams }.render().unwrap())
}

#[get("/player/enqueue")]
async fn enqueue(query: Query<StreamQuery>) -> Html {
    Html(
        templates::Enqueue {
            stream: &query.stream,
        }
        .render()
        .unwrap(),
    )
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
