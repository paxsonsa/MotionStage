use actix_web::{middleware, web, App, HttpRequest, HttpResponse, HttpServer};
use actix_ws::{AggregatedMessage, CloseReason, MessageStream, Session};
use env_logger::Env;
use futures::StreamExt;
use std::pin::pin;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{debug, error, info, warn, Instrument};
use webrtc::media::audio::buffer::info;

use crate::rt;
use crate::session::SessionEvent;

/// Configuration constants for heartbeat intervals and client timeouts.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors that can occur while running the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("Failed to bind server to address")]
    BindError(#[from] std::io::Error),
}

/// Struct for configuring and running the server.
#[derive(typed_builder::TypedBuilder)]
#[builder(field_defaults(setter(prefix = "with_")))]
pub struct Server {
    bind_address: std::net::SocketAddr,
}

impl Server {
    /// Runs the server until it is stopped.
    pub async fn run(self, runtime: rt::RuntimeHandle) -> Result<(), ServerError> {
        env_logger::init_from_env(Env::default().default_filter_or("info"));
        info!(%self.bind_address, "Initializing HTTP server.");

        HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(runtime.clone()))
                .route("/ws", web::get().to(handle_ws_route))
                .wrap(middleware::Logger::default())
        })
        .bind(self.bind_address)
        .map_err(|err| {
            error!(%self.bind_address, error = ?err, "Failed to bind server.");
            ServerError::BindError(err)
        })?
        .run()
        .await?;

        info!(%self.bind_address, "Server is running.");
        Ok(())
    }
}

/// Handles WebSocket connections and delegates message processing.
async fn handle_ws_route(
    req: HttpRequest,
    body: web::Payload,
    runtime: web::Data<rt::RuntimeHandle>,
) -> Result<HttpResponse, actix_web::Error> {
    let ip_addr = req
        .connection_info()
        .peer_addr()
        .unwrap_or("unknown")
        .to_string();
    let initial_span = tracing::info_span!("websocket", %ip_addr);

    let _entered = initial_span.enter();
    debug!(%ip_addr, "Establishing WebSocket connection.");

    let (response, session, msg_stream) = actix_ws::handle(&req, body).map_err(|err| {
        error!(%ip_addr, error = ?err, "Failed to initialize WebSocket session.");
        err
    })?;

    let ip_addr_ = ip_addr.clone();
    tokio::task::spawn_local(
        async move {
            handler_websocket_session(runtime, session, msg_stream, ip_addr).await;
        }
        .instrument(initial_span.clone()),
    );

    info!(%ip_addr_, "WebSocket connection established.");
    Ok(response)
}

/// Manages WebSocket sessions, including heartbeats and message handling.
async fn handler_websocket_session(
    runtime: web::Data<rt::RuntimeHandle>,
    mut ws_session: Session,
    msg_stream: MessageStream,
    ip_addr: String,
) {
    let mut last_heartbeat = Instant::now();
    let mut heartbeat_interval = interval(HEARTBEAT_INTERVAL);
    let mut session = runtime.new_session().await;
    let session_id = session.id;
    info!(%ip_addr, %session_id, "Established client session.");

    // Create a combined span with IP and session_id
    let span = tracing::info_span!("session", %session_id);

    let _ = async move {
        info!("WebSocket session started.");

        let mut stream = pin!(msg_stream
            .max_frame_size(128 * 1024)
            .aggregate_continuations()
            .max_continuation_size(2 * 1024 * 1024));

        let close_reason = loop {
            tokio::select! {
                // Handle heartbeat interval
                _ = heartbeat_interval.tick() => {
                    if last_heartbeat.elapsed() > CLIENT_TIMEOUT {
                        warn!("Client heartbeat timeout. Closing session.");
                        break None;
                    }
                    if let Err(err) = ws_session.ping(b"").await {
                        warn!(error = ?err, "Failed to send heartbeat ping. Closing session.");
                        break None;
                    }
                    debug!("Heartbeat ping sent.");
                }

                // Handle session events
                maybe_event = session.event_stream.recv() => {
                    match maybe_event {
                        Some(SessionEvent::Message(msg)) => {
                            debug!("Received session event message.");
                            if let Err(err) = ws_session.text(msg).await {
                                error!(error = ?err, "Failed to send message to client. Closing session.");
                                break None;
                            }
                        }
                        None => {
                            info!("Session event stream ended. Closing session.");
                            break None;
                        }
                    }
                }

                // Handle WebSocket messages
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        Some(Ok(msg)) => {
                            debug!("Processing WebSocket message.");
                            if let Err(reason) = process_message(&mut ws_session, &mut last_heartbeat, &mut session, msg).await {
                                warn!(?reason, "Closing session due to WebSocket message handling error.");
                                break reason;
                            }
                        }
                        Some(Err(err)) => {
                            error!(error = ?err, "Error reading WebSocket message. Closing session.");
                            break None;
                        }
                        None => {
                            info!("WebSocket message stream ended. Closing session.");
                            break None;
                        }
                    }
                }
            }
        };

        // Close the session cleanly with the close reason
        session.close().await;
        info!(close_reason = ?close_reason, "Session closed.");
        let _ = ws_session.close(close_reason).await;
    }.instrument(span).await;
}

/// Processes incoming WebSocket messages and updates session state.
/// Returns `Ok(())` on success or a `CloseReason` if the session should be closed.
async fn process_message(
    ws_session: &mut Session,
    last_heartbeat: &mut Instant,
    session: &mut crate::session::SessionHandle,
    msg: AggregatedMessage,
) -> Result<(), Option<CloseReason>> {
    match msg {
        AggregatedMessage::Text(text) => {
            debug!("Received text message.");
            ws_session.text(text).await.map_err(|err| {
                error!(error = ?err, "Failed to send text message.");
                None
            })?;
        }
        AggregatedMessage::Binary(bin) => {
            debug!("Received binary message.");
            ws_session.binary(bin).await.map_err(|err| {
                error!(error = ?err, "Failed to send binary message.");
                None
            })?;
        }
        AggregatedMessage::Ping(bytes) => {
            debug!("Received ping message.");
            *last_heartbeat = Instant::now();
            session.ping().await;
            ws_session.pong(&bytes).await.map_err(|err| {
                error!(error = ?err, "Failed to send pong response.");
                None
            })?;
        }
        AggregatedMessage::Pong(_) => {
            debug!("Received pong message.");
            *last_heartbeat = Instant::now();
            session.pong().await;
        }
        AggregatedMessage::Close(reason) => {
            info!("Client requested close: {:?}", reason);
            return Err(reason); // Return the close reason
        }
    }
    Ok(())
}
