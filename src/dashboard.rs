use std::time::{Duration, Instant};

use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{
    get,
    web::{Data, Payload, Query},
    HttpRequest, HttpResponse, Result,
};
use actix_web_actors::ws;
use askama::Template;
use sqlx::MySqlPool;
use uuid::Uuid;

use crate::{auth_as_streamer, manager, ome_statistics, Config, Credentials, Html};

mod templates {
    use askama_actix::Template;
    use uuid::Uuid;

    use crate::{manager, OmeStatisticsResponse};

    #[derive(Template)]
    #[template(path = "dashboard/index.html")]
    pub struct Index;

    #[derive(Template)]
    #[template(path = "dashboard/dashboard.html")]
    pub struct Dashboard<'a> {
        pub id: &'a str,
        pub key: &'a str,
    }

    #[derive(Template)]
    #[template(path = "dashboard/queue.html")]
    pub struct Queue<'a> {
        pub queue: Vec<QueuedConnection<'a>>,
    }

    pub struct QueuedConnection<'a> {
        pub address: &'a str,
        pub uuid: Uuid,
        pub new: bool,
        pub allow: manager::Handle,
        pub deny: manager::Handle,
    }

    #[derive(Template)]
    #[template(path = "dashboard/statistics.html")]
    pub struct Statistics {
        pub statistics: Option<OmeStatisticsResponse>,
    }
}

pub struct QueueWebSocket {
    hb: Instant,
    addr: Addr<manager::Manager>,
    stream: String,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Refresh {
    pub queue: Vec<(Uuid, manager::QueuedPlayer)>,
    pub new: Option<Uuid>,
}

impl QueueWebSocket {
    const INTERVAL: Duration = Duration::from_secs(1);
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    pub fn new(addr: Addr<manager::Manager>, stream: String) -> Self {
        Self {
            addr,
            stream,
            hb: Instant::now(),
        }
    }

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
                act.addr.do_send(manager::RecipientDisconnected {
                    recipient: ctx.address().recipient(),
                    stream: act.stream.clone(),
                });
                ctx.stop();
            } else {
                ctx.ping(&[]);
            }
        });
    }

    fn refresh(
        &self,
        _queue: &[(Uuid, manager::QueuedPlayer)],
        new: Option<Uuid>,
        ctx: &mut ws::WebsocketContext<Self>,
    ) {
        let _queue = _queue
            .iter()
            .map(|(uuid, queued_player)| templates::QueuedConnection {
                address: &queued_player.ip_addr,
                uuid: *uuid,
                new: new.map(|_uuid| _uuid == *uuid).unwrap_or(false),
                allow: manager::Handle {
                    uuid: *uuid,
                    allow: true,
                    stream: self.stream.clone(),
                },
                deny: manager::Handle {
                    uuid: *uuid,
                    allow: false,
                    stream: self.stream.clone(),
                },
            })
            .collect::<Vec<_>>();

        ctx.text(templates::Queue { queue: _queue }.render().unwrap())
    }
}

impl Actor for QueueWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);

        self.addr.do_send(manager::NewRecipient {
            stream: self.stream.clone(),
            recipient: ctx.address().recipient(),
        });
    }

    fn stopped(&mut self, ctx: &mut Self::Context) {
        self.addr.do_send(manager::RecipientDisconnected {
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
            Ok(ws::Message::Text(text)) => {
                self.addr
                    .do_send(serde_json::from_str::<manager::Handle>(&text).unwrap());
            }
            Ok(ws::Message::Pong(_)) => self.hb = Instant::now(),
            _ => ctx.stop(),
        }
    }
}

impl Handler<Refresh> for QueueWebSocket {
    type Result = <Refresh as Message>::Result;

    fn handle(&mut self, msg: Refresh, ctx: &mut Self::Context) -> Self::Result {
        self.refresh(&msg.queue, msg.new, ctx);
    }
}
struct StatisticsWebSocket {
    hb: Instant,
    config: Data<Config>,
    agent: Data<ureq::Agent>,
    stream: String,
}

impl StatisticsWebSocket {
    const INTERVAL: Duration = Duration::from_secs(1);
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    fn new(config: Data<Config>, client: Data<ureq::Agent>, stream: String) -> Self {
        Self {
            hb: Instant::now(),
            config,
            agent: client,
            stream,
        }
    }

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
                ctx.stop();

                return;
            }

            ctx.ping(&[]);

            ctx.text(
                templates::Statistics {
                    statistics: ome_statistics(act.config.clone(), act.agent.clone(), &act.stream)
                        .ok(),
                }
                .render()
                .unwrap(),
            );
        });
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for StatisticsWebSocket {
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

impl Actor for StatisticsWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);
    }
}
#[get("/dashboard/queue")]
async fn queue(
    req: HttpRequest,
    payload: Payload,
    pool: Data<MySqlPool>,
    addr: Data<Addr<manager::Manager>>,
    credentials: Query<Credentials>,
) -> Result<HttpResponse> {
    auth_as_streamer(pool, &credentials.id, &credentials.key).await?;

    ws::start(
        QueueWebSocket::new(addr.get_ref().clone(), credentials.id.clone()),
        &req,
        payload,
    )
}

#[get("/dashboard/statistics")]
async fn statistics(
    req: HttpRequest,
    payload: Payload,
    config: Data<Config>,
    client: Data<ureq::Agent>,
    pool: Data<MySqlPool>,
    credentials: Query<Credentials>,
) -> Result<HttpResponse> {
    auth_as_streamer(pool, &credentials.id, &credentials.key).await?;

    ws::start(
        StatisticsWebSocket::new(config.clone(), client, credentials.id.clone()),
        &req,
        payload,
    )
}

#[get("/dashboard/dashboard")]
async fn dashboard(pool: Data<MySqlPool>, credentials: Query<Credentials>) -> Result<Html> {
    auth_as_streamer(pool, &credentials.id, &credentials.key).await?;

    Ok(Html(
        templates::Dashboard {
            id: &credentials.id,
            key: &credentials.key,
        }
        .render()
        .unwrap(),
    ))
}

#[get("/dashboard")]
async fn index() -> Html {
    Html(templates::Index.render().unwrap())
}
