use actix_web::{get, HttpResponse};

#[get("/static/notification")]
async fn notification() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("audio/wav")
        .body(&include_bytes!("../res/Oi.wav")[..])
}

#[get("/static/haroldium")]
async fn haroldium() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("image/gif")
        .body(&include_bytes!("../res/haroldium.gif")[..])
}

#[get("/static/killmeplz")]
async fn killmeplz() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("image/webp")
        .body(&include_bytes!("../res/killmeplz.webp")[..])
}
