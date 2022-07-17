use actix_files::Files;
use actix_web::{get, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Data {
    last_requests: Mutex<HashMap<String, Instant>>,
}

const WAIT_LIMIT: Duration = Duration::from_secs(1);

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

#[get("/ip")]
async fn get_ip(req: HttpRequest) -> impl Responder {
    let ip = req.connection_info().realip_remote_addr().unwrap().to_string();
    HttpResponse::Ok()
        .content_type("text/plain")
        .body(ip)
}

#[get("/check")]
async fn check_port(req: web::Query<PortRequest>, data: web::Data<Data>, request: HttpRequest) -> impl Responder {
    let req_ip = request.connection_info().realip_remote_addr().unwrap().to_string();
    let target_ip = req.ip.as_ref().unwrap_or(&req_ip);
    {
        let mut requests = data.last_requests.lock().unwrap();
        let last = requests.get(target_ip);
        if let Some(last) = last {
            if last.elapsed() < WAIT_LIMIT {
                return HttpResponse::TooManyRequests()
                    .content_type("text/plain")
                    .body(format!(
                        "&#128337; Wait at least {} seconds for the next request!",
                        WAIT_LIMIT.as_secs()
                    ));
            }
        }
        requests.insert(target_ip.clone(), Instant::now());
    }

    let socket = UdpSocket::bind(("::", 0));
    if let Err(e) = socket {
        return HttpResponse::InternalServerError()
            .content_type("text/plain")
            .body(format!("Failed to bind socket: {e}"));
    }

    let socket = socket.unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();

    let response = socket
        .send_to(&CONNECT_PACKET, (target_ip.clone(), req.port));
    match response {
        Ok(_) => {
            let mut buf = [0; 100];
            let mut response = "&cross; Connection doesn't work: Failed to receive response.";
            while let Ok((num, _)) = socket.recv_from(&mut buf) {
                if num == 0 {
                    continue;
                }
                match buf[0] {
                    // Received correct response
                    0x06 => {
                        let connection_number = buf[1];

                        let mut disconnect = DISCONNECT_PACKET;
                        disconnect[0] |= connection_number << 5; // Insert connection number

                        let _ = socket.send_to(&disconnect, (target_ip.clone(), req.port));
                        response = "&check; Connection works!";

                        return HttpResponse::Ok().content_type("text/plain").body(response);
                    }
                    // Received either shutdown ok, ping or mtu check, ignore...
                    0x10 | 0x03 | 0x0c => (),
                    // Everything else is considered an error
                    _ => {
                        response = "&cross; Connection doesn't work: Received incorrect packet";
                    }
                }
            }
            HttpResponse::ServiceUnavailable().content_type("text/plain").body(response)
        }
        Err(e) => HttpResponse::InternalServerError()
            .content_type("text/plain")
            .body(format!("&cross; Failed to send request: {}", e)),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let data = Data {
        last_requests: Mutex::new(HashMap::new()),
    };

    let data = web::Data::new(data);

    HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            .service(web::scope("/api").service(check_port).service(get_ip))
            .service(Files::new("/", "static").index_file("index.html"))
    })
    .bind("127.0.0.1:8080")?
    .shutdown_timeout(30)
    .run()
    .await
}
