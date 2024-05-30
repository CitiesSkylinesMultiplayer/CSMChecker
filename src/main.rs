use actix_files::Files;
use actix_web::middleware::Logger;
use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use chrono::{DateTime, Utc};
use env_logger::Env;
use reqwest::header::USER_AGENT;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::Mutex;

struct LatestVersion {
    version: String,
    updated: DateTime<Utc>,
    pending: bool,
}

struct Data {
    latest_version: Mutex<LatestVersion>,
}

const VERSION_INTERVAL: Duration = Duration::from_secs(5 * 60);
const VERSION_MAX_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24);

#[derive(Serialize, Deserialize)]
struct GithubRelease {
    tag_name: String,
    published_at: DateTime<Utc>,
}

async fn request_version() -> reqwest::Result<Option<String>> {
    let releases = reqwest::Client::new()
        .request(
            Method::GET,
            "https://api.github.com/repos/CitiesSkylinesMultiplayer/CSM/releases",
        )
        .header(USER_AGENT, "csm-checker")
        .send()
        .await?
        .json::<Vec<GithubRelease>>()
        .await?;
    if releases.is_empty() {
        Ok(None)
    } else {
        let release = releases.get(0).unwrap();
        Ok(Some(release.tag_name.clone()))
    }
}

#[get("/version")]
async fn get_version(data: web::Data<Data>) -> impl Responder {
    {
        let mut latest_version = data.latest_version.lock().await;
        if latest_version.pending
            || (!latest_version.version.is_empty()
                && Utc::now() - latest_version.updated
                    < chrono::Duration::from_std(VERSION_INTERVAL).unwrap())
        {
            return HttpResponse::Ok()
                .content_type("text/plain")
                .body(latest_version.version.clone());
        }
        latest_version.pending = true;
    }
    let response = request_version().await;
    let mut latest_version = data.latest_version.lock().await;
    latest_version.pending = false;
    match response {
        Ok(Some(version)) => {
            latest_version.version = version.clone();
            latest_version.updated = Utc::now();
            HttpResponse::Ok().content_type("text/plain").body(version)
        }
        Ok(None) => HttpResponse::InternalServerError()
            .content_type("text/plain")
            .body("No releases found"),
        Err(e) => {
            if !latest_version.version.is_empty()
                && Utc::now() - latest_version.updated
                    < chrono::Duration::from_std(VERSION_MAX_LIFETIME).unwrap()
            {
                HttpResponse::Ok()
                    .content_type("text/plain")
                    .body(latest_version.version.clone())
            } else {
                HttpResponse::InternalServerError()
                    .content_type("text/plain")
                    .body(format!("Failed to request version: {e}"))
            }
        }
    }
}

#[get("/ip")]
async fn get_ip(req: HttpRequest) -> impl Responder {
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    HttpResponse::Ok().content_type("text/plain").body(ip)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let data = Data {
        latest_version: Mutex::new(LatestVersion {
            version: String::new(),
            updated: Utc::now(),
            pending: false,
        }),
    };

    let data = web::Data::new(data);

    env_logger::init_from_env(Env::new().default_filter_or("info"));

    HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            .wrap(Logger::new("%r -> %s (Took %Ts)"))
            .service(web::scope("/api").service(get_ip).service(get_version))
            .service(Files::new("/", "static").index_file("index.html"))
    })
    .workers(2)
    .bind("127.0.0.1:8080")?
    .shutdown_timeout(15)
    .run()
    .await
}
