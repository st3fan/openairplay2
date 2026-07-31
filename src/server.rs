//! AirPlay 2 control server: accept loop and request dispatch.
//!
//! Milestone 1 answers `GET /info` with the device plist and logs every other
//! request (returning `501`) so the sender's real request sequence — in
//! particular the pairing handshake — is visible for milestone 2.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use log::{debug, info, warn};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};

use crate::http::{read_request, Request, Response};
use crate::identity::Identity;
use crate::info::info_plist;
use crate::Config;

pub const SERVER_ID: &str = "AirTunes/366.0";
pub const INFO_CONTENT_TYPE: &str = "application/x-apple-binary-plist";

pub struct Context {
    pub config: Config,
    pub identity: Identity,
}

pub async fn serve(listener: TcpListener, context: Arc<Context>) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let context = context.clone();
        tokio::spawn(async move {
            info!("[{peer}] connected");
            if let Err(e) = handle_connection(stream, peer, context).await {
                warn!("[{peer}] connection error: {e}");
            }
            info!("[{peer}] disconnected");
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    context: Arc<Context>,
) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    while let Some(request) = read_request(&mut reader).await? {
        log_request(&peer, &request);
        let response = dispatch(&request, &context);
        debug!("[{peer}] -> {} {}", request.method, response.status());
        response.write_to(&mut write_half).await?;
    }
    Ok(())
}

fn log_request(peer: &SocketAddr, request: &Request) {
    info!(
        "[{peer}] {} {} {}",
        request.method, request.target, request.protocol
    );
    for (name, value) in request.headers.iter() {
        debug!("[{peer}]   {name}: {value}");
    }
    if !request.body.is_empty() {
        debug!("[{peer}]   body: {} bytes", request.body.len());
    }
}

fn dispatch(request: &Request, context: &Context) -> Response {
    let proto = &request.protocol;
    let mut response = match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/info") => Response::ok(proto).body(
            INFO_CONTENT_TYPE,
            info_plist(&context.config, &context.identity),
        ),
        (method, target) => {
            warn!("{method} {target} not implemented yet");
            Response::new(proto, 501, "Not Implemented")
        }
    };

    // AirPlay echoes CSeq and carries a Server header.
    if let Some(cseq) = request.headers.get("CSeq") {
        response = response.header("CSeq", cseq);
    }
    response.header("Server", SERVER_ID)
}
