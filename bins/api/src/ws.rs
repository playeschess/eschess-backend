//! Live play over a WebSocket at `GET /v1/ws`.
//!
//! The conversation, every message a JSON object with a `type`:
//!
//! ```text
//! client -> HELLO        {protocol_version, access_token}     must be the first message, within a few seconds
//! server -> WELCOME      {protocol_version, user_id, server_time_ms}
//! client -> SUBSCRIBE    {game_id, last_event_seq?}           watch a game you may see
//! server -> GAME_SNAPSHOT {game_id, event_seq, unchanged, game}
//! server -> GAME_UPDATE  {event_seq, game}                    every time the game changes, whoever changed it
//! client -> MOVE_REQUEST {game_id, uci, client_move_id}
//! server -> MOVE_ACCEPTED {client_move_id, game_id, ply, uci, replayed, charged_ms, credited_ms, event_seq}
//!        or MOVE_REJECTED {client_move_id, game_id, code, message}
//! server -> CLOCK_SYNC   {game_id, event_seq, turn, clock}    every few seconds, to correct a drifting clock
//! server -> PING {id, server_time_ms}    client -> PONG {id}  the server times the round trip to measure lag
//! server -> ERROR        {code, message, game_id?}
//! ```
//!
//! The access token travels in HELLO, not in the URL (URLs end up in logs). The server never believes anything the
//! client says about identity, the position, whose turn it is or how long it has: a move request names only a game and
//! a move. A client that sees a gap in `event_seq` has missed an update and re-sends SUBSCRIBE for a fresh snapshot.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use eschess_auth::token::hash_token;
use eschess_storage::{games, identity};
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::ApiError;
use crate::hub::SubscribeError;
use crate::routes::games::game_json;
use crate::AppState;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "HELLO")]
    Hello {
        protocol_version: u32,
        access_token: String,
    },
    #[serde(rename = "SUBSCRIBE")]
    Subscribe {
        game_id: Uuid,
        last_event_seq: Option<i32>,
    },
    #[serde(rename = "UNSUBSCRIBE")]
    Unsubscribe { game_id: Uuid },
    #[serde(rename = "MOVE_REQUEST")]
    MoveRequest {
        game_id: Uuid,
        uci: String,
        client_move_id: Uuid,
    },
    #[serde(rename = "PONG")]
    Pong { id: u64 },
    #[serde(rename = "PING")]
    Ping { id: Option<u64> },
}

fn error_message(code: &str, message: &str, game_id: Option<Uuid>) -> String {
    json!({ "type": "ERROR", "code": code, "message": message, "game_id": game_id }).to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub async fn upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let max = state.config.ws_max_message_bytes;
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| connection(state, socket))
}

async fn connection(state: AppState, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    // Everything this connection sends goes through one bounded queue, drained by a single writer task.
    let (out, mut queue) = mpsc::channel::<String>(64);
    let writer = tokio::spawn(async move {
        while let Some(text) = queue.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        // Every sender is gone: say goodbye properly once everything queued has been written.
        let _ = sink.send(Message::Close(None)).await;
    });
    run(&state, &mut stream, out).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

/// The next text message, or `None` when the connection is over. Binary frames come back as an empty string, which is
/// then refused as a bad message; WebSocket-level ping and pong frames are handled by the library.
async fn next_text(stream: &mut SplitStream<WebSocket>) -> Option<String> {
    loop {
        match stream.next().await? {
            Ok(Message::Text(t)) => return Some(t.as_str().to_owned()),
            Ok(Message::Binary(_)) => return Some(String::new()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

struct Live {
    conn: u64,
    user: Uuid,
    token_hash: String,
    pings: HashMap<u64, Instant>,
    next_ping: u64,
    /// Recent round-trip times, in milliseconds, measured by the server.
    rtts: VecDeque<u64>,
    last_pong: Instant,
    window_start: Instant,
    in_window: u32,
    bad_messages: u32,
    last_reauth: Instant,
    last_sync: Instant,
}

impl Live {
    /// The lag to refund on a move: half the median recent round trip. The server measures it itself, from when it sent
    /// a ping to when the answer came back. A client could delay its answers to inflate it, which is why the clock only
    /// ever refunds a little (a per-move cap, from a small quota that refills slowly).
    fn lag_ms(&self) -> u64 {
        if self.rtts.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u64> = self.rtts.iter().copied().collect();
        sorted.sort_unstable();
        sorted[sorted.len() / 2] / 2
    }
}

enum Next {
    Continue,
    Close,
}

async fn run(state: &AppState, stream: &mut SplitStream<WebSocket>, out: mpsc::Sender<String>) {
    let cfg = &state.config;

    // 1. The first thing a client must do is say HELLO and prove who it is.
    let first = tokio::time::timeout(
        Duration::from_millis(cfg.ws_hello_timeout_ms),
        next_text(stream),
    )
    .await;
    let text = match first {
        Ok(Some(t)) => t,
        Ok(None) => return,
        Err(_) => {
            let _ = out.try_send(error_message(
                "hello_timeout",
                "send HELLO with your access token first",
                None,
            ));
            return;
        }
    };
    let (version, token) = match serde_json::from_str::<ClientMessage>(&text) {
        Ok(ClientMessage::Hello {
            protocol_version,
            access_token,
        }) => (protocol_version, access_token),
        _ => {
            let _ = out.try_send(error_message(
                "hello_required",
                "the first message must be HELLO {protocol_version, access_token}",
                None,
            ));
            return;
        }
    };
    if version != PROTOCOL_VERSION {
        let _ = out.try_send(error_message(
            "unsupported_version",
            &format!("this server speaks protocol version {PROTOCOL_VERSION}"),
            None,
        ));
        return;
    }
    let token_hash = hash_token(token.trim());
    let Ok(Some(me)) = identity::authenticate(&state.pool, &token_hash).await else {
        let _ = out.try_send(error_message("unauthorized", "sign in again", None));
        return;
    };
    tracing::info!(
        "websocket connected: user {} speaking protocol version {version}",
        me.user_id
    );

    let conn = state.hub.register(me.user_id, out.clone());
    state.hub.send(conn, json!({ "type": "WELCOME", "protocol_version": PROTOCOL_VERSION, "user_id": me.user_id, "server_time_ms": now_ms() }).to_string());
    drop(out); // the hub holds the queue's sender now; removing the connection from the hub closes it

    let now = Instant::now();
    let mut live = Live {
        conn,
        user: me.user_id,
        token_hash,
        pings: HashMap::new(),
        next_ping: 1,
        rtts: VecDeque::new(),
        last_pong: now,
        window_start: now,
        in_window: 0,
        bad_messages: 0,
        last_reauth: now,
        last_sync: now,
    };
    let mut ping_timer = tokio::time::interval(Duration::from_millis(cfg.ws_ping_interval_ms));
    let mut tick_timer = tokio::time::interval(Duration::from_millis(cfg.ws_tick_ms));

    loop {
        if !state.hub.is_registered(conn) {
            break; // dropped by the hub (it could not keep up)
        }
        let step = tokio::select! {
            incoming = stream.next() => match incoming {
                None | Some(Err(_)) => Next::Close,
                Some(Ok(Message::Text(t))) => on_text(state, &mut live, t.as_str()).await,
                Some(Ok(Message::Binary(_))) => bad_message(state, &mut live, "send JSON text messages only"),
                Some(Ok(Message::Close(_))) => Next::Close,
                Some(Ok(_)) => Next::Continue,
            },
            _ = ping_timer.tick() => on_ping_timer(state, &mut live),
            _ = tick_timer.tick() => on_tick(state, &mut live).await,
        };
        if matches!(step, Next::Close) {
            break;
        }
    }
    state.hub.remove(conn);
}

fn bad_message(state: &AppState, live: &mut Live, why: &str) -> Next {
    live.bad_messages += 1;
    state
        .hub
        .send(live.conn, error_message("bad_message", why, None));
    if live.bad_messages > 5 {
        Next::Close
    } else {
        Next::Continue
    }
}

async fn signed_in(state: &AppState, live: &Live) -> bool {
    matches!(
        identity::authenticate(&state.pool, &live.token_hash).await,
        Ok(Some(_))
    )
}

async fn on_text(state: &AppState, live: &mut Live, text: &str) -> Next {
    // A client that floods us is cut off.
    let now = Instant::now();
    if now.duration_since(live.window_start) >= Duration::from_secs(1) {
        live.window_start = now;
        live.in_window = 0;
    }
    live.in_window += 1;
    if live.in_window > state.config.ws_max_messages_per_second {
        state.hub.send(
            live.conn,
            error_message("rate_limited", "too many messages", None),
        );
        return Next::Close;
    }

    let message = match serde_json::from_str::<ClientMessage>(text) {
        Ok(m) => m,
        Err(_) => return bad_message(state, live, "that is not a message this server understands"),
    };
    match message {
        ClientMessage::Hello { .. } => bad_message(state, live, "already said HELLO"),
        ClientMessage::Ping { id } => {
            state
                .hub
                .send(live.conn, json!({ "type": "PONG", "id": id }).to_string());
            Next::Continue
        }
        ClientMessage::Pong { id } => {
            if let Some(sent) = live.pings.remove(&id) {
                live.rtts.push_back(sent.elapsed().as_millis() as u64);
                if live.rtts.len() > 5 {
                    live.rtts.pop_front();
                }
                live.last_pong = Instant::now();
            }
            Next::Continue
        }
        ClientMessage::Subscribe {
            game_id,
            last_event_seq,
        } => {
            match state
                .hub
                .subscribe(live.conn, game_id, state.config.ws_max_subscriptions)
            {
                Err(SubscribeError::TooMany) => {
                    state.hub.send(
                        live.conn,
                        error_message(
                            "too_many_subscriptions",
                            "unsubscribe from a game first",
                            Some(game_id),
                        ),
                    );
                    return Next::Continue;
                }
                Err(SubscribeError::Gone) => return Next::Close,
                Ok(()) => {}
            }
            // Subscribed first, read second: a change in between is delivered as an update, never lost.
            match games::get_game(&state.pool, game_id, live.user).await {
                Ok(view) => {
                    let unchanged = last_event_seq == Some(view.event_seq);
                    state.hub.send(live.conn, json!({ "type": "GAME_SNAPSHOT", "game_id": game_id, "event_seq": view.event_seq, "unchanged": unchanged, "game": game_json(&view) }).to_string());
                }
                Err(e) => {
                    state.hub.unsubscribe(live.conn, game_id);
                    let (_, code, message) = ApiError::from(e).parts();
                    state
                        .hub
                        .send(live.conn, error_message(code, &message, Some(game_id)));
                }
            }
            Next::Continue
        }
        ClientMessage::Unsubscribe { game_id } => {
            state.hub.unsubscribe(live.conn, game_id);
            Next::Continue
        }
        ClientMessage::MoveRequest {
            game_id,
            uci,
            client_move_id,
        } => {
            if !signed_in(state, live).await {
                state.hub.send(
                    live.conn,
                    error_message(
                        "unauthorized",
                        "your session has ended; sign in again",
                        None,
                    ),
                );
                return Next::Close;
            }
            let lag = live.lag_ms();
            let reply = match games::play_move(
                &state.pool,
                game_id,
                live.user,
                uci.trim(),
                client_move_id,
                lag,
            )
            .await
            {
                Ok((view, receipt)) => json!({
                    "type": "MOVE_ACCEPTED", "client_move_id": client_move_id, "game_id": game_id, "ply": receipt.ply, "uci": receipt.uci,
                    "replayed": receipt.replayed, "charged_ms": receipt.charged_ms, "credited_ms": receipt.credited_ms, "measured_lag_ms": lag, "event_seq": view.event_seq,
                }),
                Err(e) => {
                    let (_, code, message) = ApiError::from(e).parts();
                    json!({ "type": "MOVE_REJECTED", "client_move_id": client_move_id, "game_id": game_id, "code": code, "message": message })
                }
            };
            state.hub.send(live.conn, reply.to_string());
            Next::Continue
        }
    }
}

fn on_ping_timer(state: &AppState, live: &mut Live) -> Next {
    if live.last_pong.elapsed() > Duration::from_millis(state.config.ws_pong_timeout_ms) {
        state.hub.send(
            live.conn,
            error_message("timeout", "no answer to the server's pings", None),
        );
        return Next::Close;
    }
    let id = live.next_ping;
    live.next_ping += 1;
    live.pings.insert(id, Instant::now());
    live.pings.retain(|k, _| *k + 16 > id); // forget pings that will never be answered
    state.hub.send(
        live.conn,
        json!({ "type": "PING", "id": id, "server_time_ms": now_ms() }).to_string(),
    );
    Next::Continue
}

/// Housekeeping, every tick: end the connection if the session was signed out, settle any game whose flag has fallen
/// (which pushes the result to everyone watching), and now and then send each watched game's clock.
async fn on_tick(state: &AppState, live: &mut Live) -> Next {
    let cfg = &state.config;
    if live.last_reauth.elapsed() >= Duration::from_millis(cfg.ws_reauth_ms) {
        live.last_reauth = Instant::now();
        if !signed_in(state, live).await {
            state.hub.send(
                live.conn,
                error_message(
                    "unauthorized",
                    "your session has ended; sign in again",
                    None,
                ),
            );
            return Next::Close;
        }
    }
    let watched = state.hub.subscribed_games(live.conn);
    for game in &watched {
        if games::flag_due(&state.pool, *game).await.unwrap_or(false) {
            let _ = games::get_game(&state.pool, *game, live.user).await;
        }
    }
    if live.last_sync.elapsed() >= Duration::from_millis(cfg.ws_clock_sync_ms) {
        live.last_sync = Instant::now();
        for game in watched {
            if let Ok(view) = games::peek_game(&state.pool, game, live.user).await {
                if view.status == "active" {
                    let sync: Value = json!({
                        "type": "CLOCK_SYNC", "game_id": game, "event_seq": view.event_seq, "turn": view.turn,
                        "clock": { "white_ms": view.white_ms, "black_ms": view.black_ms, "as_of_ms": view.as_of_ms },
                    });
                    state.hub.send(live.conn, sync.to_string());
                }
            }
        }
    }
    Next::Continue
}
