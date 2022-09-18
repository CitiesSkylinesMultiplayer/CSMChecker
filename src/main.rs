use actix_files::Files;
use actix_web::middleware::Logger;
use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use chrono::{DateTime, Utc};
use env_logger::Env;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use reqwest::header::USER_AGENT;
use reqwest::Method;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::timeout;

struct LatestVersion {
    version: String,
    updated: DateTime<Utc>,
    pending: bool,
}

struct Data {
    last_requests: Mutex<HashMap<String, Instant>>,
    latest_version: Mutex<LatestVersion>,
}

const VERSION_INTERVAL: Duration = Duration::from_secs(5 * 60);
const VERSION_MAX_LIFETIME: Duration = Duration::from_secs(60 * 60 * 24);
const WAIT_LIMIT: Duration = Duration::from_secs(1);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(3);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
struct PortRequest {
    ip: Option<String>,
    port: u16,
}

// Connect Packet for LiteNetLib 0.9.5.2
const CONNECT_PACKET: [u8; 37] = [
    0x05, // PacketProperty = ConnectRequest | ConnectionNumber = 0 | IsFragmented = false
    0x0B, // Protocol Id = 11
    0, 0, 0, // Unused for connect packet
    0xBA, 0xDE, 0xAF, 0xFE, 0xDE, 0xAD, 0xBE, 0xEF, // Connection Id
    0x10, // Address size?
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // Address
    0x03, 0, 0, 0, // 3 byte payload data (Byte 13-16 is string length in big endian)
    b'C', b'S', b'M',
];

// Disconnect Packet for LiteNetLib 0.9.5.2
const DISCONNECT_PACKET: [u8; 9] = [
    0x07, // Disconnect
    0xBA, 0xDE, 0xAF, 0xFE, 0xDE, 0xAD, 0xBE, 0xEF, // Connect Id
];

#[derive(Serialize, Deserialize)]
struct GithubRelease {
    tag_name: String,
    published_at: DateTime<Utc>,
}

async fn request_version() -> reqwest::Result<Option<String>> {
    let releases = reqwest::Client::new()
            .request(Method::GET, "https://api.github.com/repos/CitiesSkylinesMultiplayer/CSM/releases")
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

#[get("/check")]
async fn check_port(
    req: web::Query<PortRequest>,
    data: web::Data<Data>,
    request: HttpRequest,
) -> impl Responder {
    let req_ip = request
        .connection_info()
        .realip_remote_addr()
        .unwrap_or("")
        .to_string();
    let target_ip = req.ip.as_ref().unwrap_or(&req_ip);
    {
        let mut requests = data.last_requests.lock().await;
        let last = requests.get(target_ip);
        if let Some(last) = last {
            if last.elapsed() < WAIT_LIMIT {
                return HttpResponse::TooManyRequests()
                    .content_type("text/plain")
                    .body(format!(
                        "Wait at least {} seconds for the next request!",
                        WAIT_LIMIT.as_secs()
                    ));
            }
        }
        requests.insert(target_ip.clone(), Instant::now());
    }

    let socket = UdpSocket::bind(("::", 0)).await;
    if let Err(e) = socket {
        return HttpResponse::InternalServerError()
            .content_type("text/plain")
            .body(format!("Failed to bind socket: {e}"));
    }

    let socket = socket.unwrap();

    let response = socket.send_to(&CONNECT_PACKET, (target_ip.clone(), req.port));
    let response = timeout(SOCKET_WRITE_TIMEOUT, response).await;
    match response {
        Ok(Ok(_)) => {
            let mut buf = [0; 100];
            let mut response = "Connection doesn't work: Failed to receive response.";
            while let Ok(Ok((num, _))) =
                timeout(SOCKET_READ_TIMEOUT, socket.recv_from(&mut buf)).await
            {
                if num == 0 {
                    continue;
                }
                match buf[0] {
                    // Received correct response
                    0x06 => {
                        let connection_number = buf[1];

                        let mut disconnect = DISCONNECT_PACKET;
                        disconnect[0] |= connection_number << 5; // Insert connection number

                        let _ = timeout(
                            SOCKET_WRITE_TIMEOUT,
                            socket.send_to(&disconnect, (target_ip.clone(), req.port)),
                        )
                        .await;
                        response = "Connection works!";

                        return HttpResponse::Ok().content_type("text/plain").body(response);
                    }
                    // Received either shutdown ok, ping or mtu check, ignore...
                    0x10 | 0x03 | 0x0c => (),
                    // Everything else is considered an error
                    _ => {
                        response = "Connection doesn't work: Received incorrect packet";
                    }
                }
            }
            HttpResponse::ServiceUnavailable()
                .content_type("text/plain")
                .body(response)
        }
        Ok(Err(e)) => HttpResponse::InternalServerError()
            .content_type("text/plain")
            .body(format!("Failed to send request: {}", e)),
        Err(_) => HttpResponse::ServiceUnavailable()
            .content_type("text/plain")
            .body("Connection doesn't work: Timed out"),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let data = Data {
        last_requests: Mutex::new(HashMap::new()),
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
            .service(
                web::scope("/api")
                    .service(check_port)
                    .service(get_ip)
                    .service(get_version),
            )
            .service(Files::new("/", "static").index_file("index.html"))
    })
    .workers(12)
    .bind("127.0.0.1:8080")?
    .shutdown_timeout(15)
    .run()
    .await
}
