use std::time::{Duration, Instant};

use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{
    get,
    web::{Data, Payload, Query},
    HttpRequest, HttpResponse, Result,
};
use actix_web_actors::ws;
use sqlx::MySqlPool;
use uuid::Uuid;

use crate::{auth_as_streamer, html, manager, ome_statistics, Config, Credentials, Error, Html};

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
        let content = _queue
            .iter()
            .map(|(uuid, queued_player)| {
                let (class, _notification) = if let Some(_uuid) = new {
                    if *uuid == _uuid {
                        (r#"class="notify""#, r#"<audio src="/static/Oi.wav" autoplay="true">"#)
                    } else {
                        ("", "")
                    }
                } else {
                    ("", "")
                };
                html! {"../res/dashboard/queued_connection.html",
                    "{uuid}" => uuid,
                    "{address}" => queued_player.ip_addr,
                    "{class}" => class,
                    "{notification}" => _notification, 
                    "{allow}" => serde_json::to_string(&manager::Handle {uuid: *uuid, allow: true, stream: self.stream.clone()}).unwrap(),
                    "{deny}" => serde_json::to_string(&manager::Handle {uuid: *uuid, allow: false, stream: self.stream.clone()}).unwrap()
                }
            })
            .collect::<String>();

        ctx.text(html! {"../res/dashboard/queue.html", "{content}" => &content})
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

    fn new(config: Data<Config>, agent: Data<ureq::Agent>, stream: String) -> Self {
        Self {
            hb: Instant::now(),
            config,
            agent,
            stream,
        }
    }

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
                ctx.stop();

                return;
            }

            let config = act.config.clone();
            let agent = act.agent.clone();
            let stream = act.stream.clone();

            let content = match ome_statistics(config, agent, &stream) {
                Ok(_statistics) => html! {"../res/dashboard/statistics.html",
                    "{connections}" => _statistics.total_connections,
                    "{start_time}" => _statistics.created_time,
                    "{total_in}" => _statistics.total_bytes_in / 1_000_000,
                    "{total_out}" => _statistics.total_bytes_out / 1_000_000
                },
                Err(_) => {
                    let not_running = r#"<span class="error">Not running!</span>"#;
                    html! {"../res/dashboard/statistics.html",
                        "{connections}" => not_running,
                        "{start_time}" => not_running,
                        "{total_in}" => not_running,
                        "{total_out}" => not_running
                    }
                }
            };

            ctx.text(content);
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
    agent: Data<ureq::Agent>,
    pool: Data<MySqlPool>,
    credentials: Query<Credentials>,
) -> Result<HttpResponse> {
    auth_as_streamer(pool, &credentials.id, &credentials.key).await?;

    ws::start(
        StatisticsWebSocket::new(config.clone(), agent, credentials.id.clone()),
        &req,
        payload,
    )
}

#[get("/dashboard/dashboard")]
async fn dashboard(pool: Data<MySqlPool>, credentials: Query<Credentials>) -> Result<Html> {
    auth_as_streamer(pool, &credentials.id, &credentials.key).await?;

    Ok(Html(html! {"../res/dashboard/dashboard.html",
        "{id}" => credentials.id,
        "{key}" => credentials.key
    }))
}

#[get("/dashboard")]
async fn index() -> Html {
    Html(html! { "../res/dashboard/index.html" })
}
