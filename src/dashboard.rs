use std::time::{Duration, Instant};

use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{
    get,
    web::{Data, Payload, Query},
    HttpRequest, HttpResponse, Result,
};
use actix_web_actors::ws;
use serde::Deserialize;
use uuid::Uuid;

use crate::{html, manager, ome_statistics, Error, Html, State};

pub struct QueueWebSocket {
    hb: Instant,
    addr: Addr<manager::Manager>,
    stream: String,
}

#[derive(Deserialize)]
pub struct ApiKeyQuery {
    api_key: String,
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
                        (r#"class="notify""#, r#"<audio src="/static/notification" autoplay="true">"#)
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
    state: Data<State>,
    stream: String,
}

impl StatisticsWebSocket {
    const INTERVAL: Duration = Duration::from_secs(1);
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    fn new(state: Data<State>, stream: String) -> Self {
        Self {
            hb: Instant::now(),
            state,
            stream,
        }
    }

    fn hb(&self, ctx: &mut <Self as Actor>::Context) {
        ctx.run_interval(Self::INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.hb) > Self::CLIENT_TIMEOUT {
                ctx.stop();

                return;
            }

            let state = act.state.clone();
            let stream = act.stream.clone();

            let content = match ome_statistics(state, &stream) {
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
pub async fn queue(
    req: HttpRequest,
    payload: Payload,
    state: Data<State>,
    addr: Data<Addr<manager::Manager>>,
    query: Query<ApiKeyQuery>,
) -> Result<HttpResponse> {
    let stream = state
        .config
        .api_keys
        .get(&query.api_key)
        .ok_or(Error::Unauthorized)?
        .clone();

    ws::start(
        QueueWebSocket::new(addr.get_ref().clone(), stream),
        &req,
        payload,
    )
}

#[get("/dashboard/statistics")]
pub async fn statistics(
    req: HttpRequest,
    payload: Payload,
    state: Data<State>,
    query: Query<ApiKeyQuery>,
) -> Result<HttpResponse> {
    let stream = state
        .config
        .api_keys
        .get(&query.api_key)
        .ok_or(Error::Unauthorized)?;

    ws::start(
        StatisticsWebSocket::new(state.clone(), stream.clone()),
        &req,
        payload,
    )
}

#[get("/dashboard/dashboard")]
async fn dashboard(state: Data<State>, query: Query<ApiKeyQuery>) -> Result<Html> {
    let stream = state
        .config
        .api_keys
        .get(&query.api_key)
        .ok_or(Error::Unauthorized)?;

    Ok(Html(html! {"../res/dashboard/dashboard.html",
        "{stream}" => stream,
        "{api_key}" => query.api_key
    }))
}

#[get("/dashboard")]
async fn index() -> Html {
    Html(html! { "../res/dashboard/index.html" })
}
