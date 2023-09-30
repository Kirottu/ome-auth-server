use actix::Addr;
use actix_web::{
    get, post,
    web::{Data, Form, Query},
    HttpResponse, Result,
};
use argon2::{password_hash::SaltString, PasswordHash};
use serde::Deserialize;
use sqlx::MySqlPool;

#[derive(Deserialize)]
struct StreamIdQuery {
    stream_id: String,
}

#[derive(Deserialize)]
struct CreateStreamQuery {
    stream_id: String,
    stream_key: String,
}

use crate::{html, manager, Config, Credentials, Error, Html, StreamConfig};

/*#[get("/admin/list")]
pub async fn stream_list(pool: Data<sqlx::MySqlPool>) -> Html {
    let streams = sqlx::query_as!(StreamConfig, r#"SELECT * FROM streams"#)
        .fetch_all(&*pool.into_inner())
        .await
        .unwrap();
}*/

async fn auth_as_admin(config: Data<Config>, id: &str, key: &str) -> Result<(), Error> {
    let hash =
        PasswordHash::parse(&config.admin_key_hash, argon2::password_hash::Encoding::B64).unwrap();

    if id != config.admin_id {
        return Err(Error::Unauthorized);
    }

    hash.verify_password(&[&argon2::Argon2::default()], key)
        .map_err(|_| Error::Unauthorized)
}

#[get("/admin/create_stream_menu")]
async fn create_stream_menu(credentials: Query<Credentials>) -> Html {
    Html(html! {"../res/admin/create_stream_menu.html",
        "{id}" => credentials.id,
        "{key}" => credentials.key
    })
}

#[post("/admin/create_stream")]
async fn create_stream(
    pool: Data<MySqlPool>,
    config: Data<Config>,
    manager: Data<Addr<manager::Manager>>,
    credentials: Query<Credentials>,
    create_stream: Form<CreateStreamQuery>,
) -> Result<Html, Error> {
    auth_as_admin(config, &credentials.id, &credentials.key).await?;

    let salt = SaltString::generate(rand::thread_rng());
    let hash = PasswordHash::generate(
        argon2::Argon2::default(),
        &create_stream.stream_key,
        salt.as_salt(),
    )
    .unwrap();

    sqlx::query!(
        "INSERT INTO streams (id, key_hash) VALUES (?, ?)",
        create_stream.stream_id,
        hash.serialize().to_string()
    )
    .execute(&*pool.clone().into_inner())
    .await
    .unwrap();

    manager.do_send(manager::StreamCreated {
        stream: create_stream.stream_id.clone(),
    });

    Ok(dashboard_inner(pool, credentials).await)
}

#[post("/admin/remove_stream")]
async fn remove_stream(
    pool: Data<MySqlPool>,
    manager: Data<Addr<manager::Manager>>,
    config: Data<Config>,
    credentials: Query<Credentials>,
    stream_id: Query<StreamIdQuery>,
) -> Result<Html, Error> {
    auth_as_admin(config, &credentials.id, &credentials.key).await?;

    sqlx::query!("DELETE FROM streams WHERE id = ?", stream_id.stream_id)
        .execute(&*pool.clone().into_inner())
        .await
        .unwrap();

    manager.do_send(manager::StreamRemoved {
        stream: stream_id.stream_id.clone(),
    });

    Ok(dashboard_inner(pool, credentials).await)
}

#[get("/admin/dashboard")]
async fn dashboard(
    pool: Data<MySqlPool>,
    config: Data<Config>,
    credentials: Query<Credentials>,
) -> Result<Html, Error> {
    auth_as_admin(config, &credentials.id, &credentials.key).await?;

    Ok(dashboard_inner(pool, credentials).await)
}

async fn dashboard_inner(pool: Data<MySqlPool>, credentials: Query<Credentials>) -> Html {
    let streams = sqlx::query_as!(StreamConfig, "SELECT * FROM streams")
        .fetch_all(&*pool.into_inner())
        .await
        .unwrap()
        .into_iter()
        .map(|stream| {
            html! {"../res/admin/stream.html",
                "{id}" => credentials.id,
                "{key}" => credentials.key,
                "{stream_id}" => stream.id
            }
        })
        .collect::<String>();

    Html(html! {"../res/admin/dashboard.html",
        "{id}" => credentials.id,
        "{key}" => credentials.key,
        "{streams}" => streams
    })
}

#[get("/admin")]
async fn index() -> Html {
    Html(html! {"../res/admin/index.html"})
}
