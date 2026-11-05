//! Live play over WebSockets, against real servers on real ports and a real Postgres
//! (`make dev`, then `make test-integration`).

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use eschess_api::Config;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Quick timers, so tests that wait for the server's housekeeping do not have to wait long.
fn fast() -> Config {
    Config {
        ws_hello_timeout_ms: 400,
        ws_ping_interval_ms: 100,
        ws_pong_timeout_ms: 600,
        ws_tick_ms: 100,
        ws_clock_sync_ms: 300,
        ws_reauth_ms: 200,
        ..Config::default()
    }
}

enum Got {
    Msg(Value),
    Closed,
    Silence,
}

struct Client {
    ws: Socket,
    /// Answer the server's pings (as a real app does), after this delay.
    auto_pong: bool,
    pong_delay: Duration,
    /// Messages read while looking for another one; kept so they can still be asked for.
    backlog: Vec<Value>,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Client {
        let (ws, _) = connect_async(format!("ws://{addr}/v1/ws"))
            .await
            .expect("websocket connects");
        Client {
            ws,
            auto_pong: true,
            pong_delay: Duration::ZERO,
            backlog: Vec::new(),
        }
    }

    async fn send(&mut self, v: Value) {
        self.ws
            .send(Message::Text(v.to_string().into()))
            .await
            .unwrap();
    }

    async fn send_text(&mut self, text: String) {
        let _ = self.ws.send(Message::Text(text.into())).await;
    }

    /// The next message, answering the server's pings on the way. `Closed` if the server hung up.
    async fn next(&mut self, within: Duration) -> Got {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => return Got::Silence,
                Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => {
                    return Got::Closed
                }
                Ok(Some(Ok(Message::Text(t)))) => {
                    let m: Value = serde_json::from_str(t.as_str()).unwrap();
                    if m["type"] == "PING" && self.auto_pong {
                        tokio::time::sleep(self.pong_delay).await;
                        let _ = self
                            .ws
                            .send(Message::Text(
                                json!({ "type": "PONG", "id": m["id"] }).to_string().into(),
                            ))
                            .await;
                        continue;
                    }
                    return Got::Msg(m);
                }
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    /// Reads until a message of this type arrives, skipping the rest.
    async fn expect(&mut self, kind: &str) -> Value {
        if let Some(i) = self.backlog.iter().position(|m| m["type"] == kind) {
            return self.backlog.remove(i);
        }
        for _ in 0..200 {
            match self.next(Duration::from_secs(4)).await {
                Got::Msg(m) if m["type"] == kind => return m,
                Got::Msg(m) => {
                    if m["type"] != "CLOCK_SYNC" {
                        self.backlog.push(m);
                    }
                }
                Got::Closed => panic!("the connection closed while waiting for {kind}"),
                Got::Silence => panic!("nothing arrived while waiting for {kind}"),
            }
        }
        panic!("never saw {kind}")
    }

    /// Everything of interest that arrives in the next `ms` milliseconds (pings and clock syncs are not of interest).
    async fn quiet_for(&mut self, ms: u64) -> Vec<Value> {
        let mut seen = std::mem::take(&mut self.backlog);
        let end = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Got::Msg(m) = self
            .next(end.saturating_duration_since(tokio::time::Instant::now()))
            .await
        {
            if !["PING", "CLOCK_SYNC"].contains(&m["type"].as_str().unwrap_or("")) {
                seen.push(m);
            }
        }
        seen
    }

    async fn is_closed_soon(&mut self, ms: u64) -> bool {
        let end = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            match self
                .next(end.saturating_duration_since(tokio::time::Instant::now()))
                .await
            {
                Got::Closed => return true,
                Got::Silence => return false,
                Got::Msg(_) => continue,
            }
        }
    }

    /// Connects and signs in.
    async fn hello(addr: SocketAddr, token: &str) -> Client {
        let mut c = Client::connect(addr).await;
        c.send(json!({ "type": "HELLO", "protocol_version": 1, "access_token": token }))
            .await;
        let welcome = c.expect("WELCOME").await;
        assert_eq!(welcome["protocol_version"], 1);
        c
    }

    async fn subscribe(&mut self, game: Uuid) -> Value {
        self.send(json!({ "type": "SUBSCRIBE", "game_id": game }))
            .await;
        self.expect("GAME_SNAPSHOT").await
    }

    async fn move_request(&mut self, game: Uuid, uci: &str, client_move_id: Uuid) {
        self.send(json!({ "type": "MOVE_REQUEST", "game_id": game, "uci": uci, "client_move_id": client_move_id })).await;
    }
}

/// Two players and an active 5+0 game with `ada` as White, made through the REST API.
async fn game(api: &Api) -> (String, String, Uuid) {
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 300 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    assert_eq!(
        api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    (ada, grace, id)
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_client_that_does_not_say_hello_is_cut_off_and_a_bad_hello_is_refused(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let token = api.player("ada").await;

    // Silence: the server gives up after the hello timeout.
    let mut c = Client::connect(addr).await;
    let m = c.expect("ERROR").await;
    assert_eq!(m["code"], "hello_timeout");
    assert!(c.is_closed_soon(1_000).await);

    // Something other than HELLO first.
    let mut c = Client::connect(addr).await;
    c.send(json!({ "type": "SUBSCRIBE", "game_id": Uuid::now_v7() }))
        .await;
    assert_eq!(c.expect("ERROR").await["code"], "hello_required");
    assert!(c.is_closed_soon(1_000).await);
    let mut c = Client::connect(addr).await;
    c.send_text("this is not json".into()).await;
    assert_eq!(c.expect("ERROR").await["code"], "hello_required");

    // A token that is not valid, and a protocol version the server does not speak.
    let mut c = Client::connect(addr).await;
    c.send(json!({ "type": "HELLO", "protocol_version": 1, "access_token": "made-up" }))
        .await;
    assert_eq!(c.expect("ERROR").await["code"], "unauthorized");
    assert!(c.is_closed_soon(1_000).await);
    let mut c = Client::connect(addr).await;
    c.send(json!({ "type": "HELLO", "protocol_version": 99, "access_token": token }))
        .await;
    assert_eq!(c.expect("ERROR").await["code"], "unsupported_version");
    assert!(c.is_closed_soon(1_000).await);
    assert_eq!(
        api.state.hub.connections(),
        0,
        "none of those was ever admitted"
    );

    // The real thing: welcomed by name.
    let (_, me) = api.get("/v1/me", &token).await;
    let mut c = Client::connect(addr).await;
    c.send(json!({ "type": "HELLO", "protocol_version": 1, "access_token": token }))
        .await;
    let welcome = c.expect("WELCOME").await;
    assert_eq!(welcome["user_id"], me["id"]);
    assert!(welcome["server_time_ms"].as_u64().unwrap() > 1_700_000_000_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn subscribing_gives_a_snapshot_to_players_and_nothing_to_strangers(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let stranger = api.player("stranger").await;

    let mut a = Client::hello(addr, &ada).await;
    let snap = a.subscribe(id).await;
    assert_eq!(
        (
            snap["game"]["id"].as_str().unwrap(),
            snap["game"]["status"].as_str(),
            snap["unchanged"].as_bool()
        ),
        (id.to_string().as_str(), Some("active"), Some(false))
    );
    assert_eq!(
        snap["game"]["legal_moves"].as_array().unwrap().len(),
        20,
        "the player to move is told her moves"
    );
    let seq = snap["event_seq"].as_i64().unwrap();
    assert!(seq >= 1);

    // Asking again with the sequence number it already has: it is unchanged.
    a.send(json!({ "type": "SUBSCRIBE", "game_id": id, "last_event_seq": seq }))
        .await;
    assert_eq!(a.expect("GAME_SNAPSHOT").await["unchanged"], true);

    let mut g = Client::hello(addr, &grace).await;
    assert!(g.subscribe(id).await["game"]["legal_moves"]
        .as_array()
        .unwrap()
        .is_empty());

    // A stranger is told the game does not exist, exactly as for an id that never existed.
    let mut s = Client::hello(addr, &stranger).await;
    s.send(json!({ "type": "SUBSCRIBE", "game_id": id })).await;
    let real = s.expect("ERROR").await;
    s.send(json!({ "type": "SUBSCRIBE", "game_id": Uuid::now_v7() }))
        .await;
    let imaginary = s.expect("ERROR").await;
    assert_eq!(
        (real["code"].as_str(), imaginary["code"].as_str()),
        (Some("not_found"), Some("not_found"))
    );
    assert_eq!(real["message"], imaginary["message"]);
    assert!(
        s.quiet_for(400).await.is_empty(),
        "and the stranger is never sent anything about it"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_whole_game_is_played_live_and_both_players_see_every_change(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let (mut a, mut g) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
    );
    a.subscribe(id).await;
    g.subscribe(id).await;

    let mut last_seq = 0;
    let moves = [
        ("f2f3", true),
        ("e7e5", false),
        ("g2g4", true),
        ("d8h4", false),
    ];
    for (i, (uci, white)) in moves.into_iter().enumerate() {
        let (mover, other) = if white {
            (&mut a, &mut g)
        } else {
            (&mut g, &mut a)
        };
        mover.move_request(id, uci, Uuid::now_v7()).await;
        let accepted = mover.expect("MOVE_ACCEPTED").await;
        assert_eq!(
            (
                accepted["ply"].as_i64(),
                accepted["uci"].as_str(),
                accepted["replayed"].as_bool()
            ),
            (Some(i as i64 + 1), Some(uci), Some(false))
        );
        // Both players are pushed the new state; the sequence number only ever goes up.
        let (theirs, mine) = (
            other.expect("GAME_UPDATE").await,
            mover.expect("GAME_UPDATE").await,
        );
        assert_eq!(theirs["game"]["ply"], i as i64 + 1);
        assert_eq!(mine["event_seq"], theirs["event_seq"]);
        let seq = theirs["event_seq"].as_i64().unwrap();
        assert!(seq > last_seq, "{seq} should be after {last_seq}");
        last_seq = seq;
        // Each is told their moves only when it is their turn.
        if i < 3 {
            assert!(
                !theirs["game"]["legal_moves"].as_array().unwrap().is_empty(),
                "after {uci}, the opponent is told their moves"
            );
            assert!(mine["game"]["legal_moves"].as_array().unwrap().is_empty());
        }
    }
    // The last move was checkmate, and the result arrived in that same update.
    let (_, end) = api.get(&format!("/v1/games/{id}"), &ada).await;
    assert_eq!(
        (
            end["status"].as_str(),
            end["result"].as_str(),
            end["end_reason"].as_str()
        ),
        (Some("finished"), Some("black"), Some("checkmate"))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn changes_made_over_plain_http_reach_connected_players_too(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let (mut a, mut g) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
    );
    a.subscribe(id).await;
    g.subscribe(id).await;

    let (status, _) = api
        .post(
            &format!("/v1/games/{id}/moves"),
            &ada,
            json!({ "uci": "e2e4", "client_move_id": Uuid::now_v7() }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    for c in [&mut a, &mut g] {
        let u = c.expect("GAME_UPDATE").await;
        assert_eq!(
            (u["game"]["ply"].as_i64(), u["game"]["turn"].as_str()),
            (Some(1), Some("black"))
        );
    }
    // A resignation made over HTTP is pushed as well.
    api.post(&format!("/v1/games/{id}/resign"), &grace, json!({}))
        .await;
    let u = a.expect("GAME_UPDATE").await;
    assert_eq!(
        (
            u["game"]["status"].as_str(),
            u["game"]["end_reason"].as_str()
        ),
        (Some("finished"), Some("resignation"))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_creator_is_told_the_moment_an_opponent_joins(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 60 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    let mut a = Client::hello(addr, &ada).await;
    assert_eq!(a.subscribe(id).await["game"]["status"], "waiting");

    api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    let u = a.expect("GAME_UPDATE").await;
    assert_eq!(
        (
            u["game"]["status"].as_str(),
            u["game"]["black"]["username"].as_str()
        ),
        (Some("active"), Some("grace"))
    );
    assert_eq!(
        u["game"]["legal_moves"].as_array().unwrap().len(),
        20,
        "and it is already her turn to move"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_retried_move_over_the_socket_is_answered_again_and_never_played_twice(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let (mut a, mut g) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
    );
    a.subscribe(id).await;
    g.subscribe(id).await;

    let cid = Uuid::now_v7();
    a.move_request(id, "e2e4", cid).await;
    assert_eq!(a.expect("MOVE_ACCEPTED").await["replayed"], false);
    g.expect("GAME_UPDATE").await;
    a.expect("GAME_UPDATE").await;

    a.move_request(id, "e2e4", cid).await; // the app did not see the answer, and sends it again
    let again = a.expect("MOVE_ACCEPTED").await;
    assert_eq!(
        (again["replayed"].as_bool(), again["ply"].as_i64()),
        (Some(true), Some(1))
    );
    assert!(
        a.quiet_for(400).await.is_empty() && g.quiet_for(100).await.is_empty(),
        "a replay changes nothing, so nobody is sent an update"
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_move_the_server_refuses_is_rejected_with_a_code_and_changes_nothing(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let stranger = api.player("stranger").await;
    let (mut a, mut g, mut s) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
        Client::hello(addr, &stranger).await,
    );
    a.subscribe(id).await;

    let cases = [
        (0, "e2e5", "illegal_move"),
        (0, "hello", "invalid_move_notation"),
        (1, "e7e5", "not_your_turn"),
        (2, "e2e4", "not_found"),
    ];
    for (who, uci, want) in cases {
        let client = match who {
            0 => &mut a,
            1 => &mut g,
            _ => &mut s,
        };
        let cid = Uuid::now_v7();
        client.move_request(id, uci, cid).await;
        let r = client.expect("MOVE_REJECTED").await;
        assert_eq!(
            (r["code"].as_str(), r["client_move_id"].as_str()),
            (Some(want), Some(cid.to_string().as_str())),
            "{uci}"
        );
    }
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 0);
    assert!(
        a.quiet_for(300).await.is_empty(),
        "nothing changed, so no updates"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_move_on_one_server_reaches_a_player_connected_to_another(pool: PgPool) {
    // A generous pong timeout: the idle client here only answers pings while it is being read.
    let server_one = Api::with_config(
        pool,
        Config {
            ws_pong_timeout_ms: 20_000,
            ..fast()
        },
    );
    let server_two = server_one.another_server();
    let (addr_one, addr_two) = (server_one.serve().await, server_two.serve().await);
    let (ada, grace, id) = game(&server_one).await;

    let mut a = Client::hello(addr_one, &ada).await;
    let mut g = Client::hello(addr_two, &grace).await;
    a.subscribe(id).await;
    g.subscribe(id).await;
    assert_eq!(
        (
            server_one.state.hub.connections(),
            server_two.state.hub.connections()
        ),
        (1, 1)
    );

    // Ada moves through server one; Grace, on server two, is told, because the change travels through Postgres.
    a.move_request(id, "e2e4", Uuid::now_v7()).await;
    a.expect("MOVE_ACCEPTED").await;
    let update = g.expect("GAME_UPDATE").await;
    assert_eq!(
        (
            update["game"]["ply"].as_i64(),
            update["game"]["moves"].to_string()
        ),
        (Some(1), json!(["e2e4"]).to_string())
    );
    assert_eq!(update["game"]["legal_moves"].as_array().unwrap().len(), 20);
    // And the other way round.
    g.move_request(id, "e7e5", Uuid::now_v7()).await;
    g.expect("MOVE_ACCEPTED").await;
    assert_eq!(a.expect("GAME_UPDATE").await["game"]["ply"], 1); // (the update for her own move, still unread)
    assert_eq!(a.expect("GAME_UPDATE").await["game"]["ply"], 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_server_measures_lag_itself_and_refunds_part_of_it(pool: PgPool) {
    let api = Api::with_config(
        pool,
        Config {
            ws_ping_interval_ms: 400,
            ws_pong_timeout_ms: 5_000,
            ..fast()
        },
    );
    let addr = api.serve().await;
    let (ada, _grace, id) = game(&api).await;
    let mut a = Client::hello(addr, &ada).await;
    a.pong_delay = Duration::from_millis(200); // this client's connection is slow: each answer takes 200 ms
    a.subscribe(id).await;
    a.quiet_for(1_700).await; // let the server take a few measurements

    // Five seconds have passed on White's clock.
    sqlx::query("UPDATE games SET turn_started_at_ms = turn_started_at_ms - 5000 WHERE id = $1")
        .bind(id)
        .execute(&api.pool)
        .await
        .unwrap();
    a.move_request(id, "e2e4", Uuid::now_v7()).await;
    let accepted = a.expect("MOVE_ACCEPTED").await;
    let (lag, credited, charged) = (
        accepted["measured_lag_ms"].as_i64().unwrap(),
        accepted["credited_ms"].as_i64().unwrap(),
        accepted["charged_ms"].as_i64().unwrap(),
    );
    assert!(
        (60..=250).contains(&lag),
        "half of a ~200 ms round trip, measured by the server: {lag}"
    );
    assert_eq!(
        credited, lag,
        "all of it refunded, being under the per-move cap and the quota"
    );
    assert!(
        (5_000 - credited..=5_000 + 3_000).contains(&charged),
        "charged {charged} after crediting {credited}"
    );

    // Over plain HTTP the server cannot measure lag, so the opponent's move over HTTP is refunded nothing.
    let (_, h) = api.get(&format!("/v1/games/{id}"), &_grace).await;
    assert_eq!(h["ply"], 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_fallen_flag_is_noticed_and_pushed_without_anyone_asking(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 10 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    let mut gr = Client::hello(addr, &grace).await;
    gr.subscribe(id).await;

    // White's time runs out while Black is just watching.
    sqlx::query("UPDATE games SET turn_started_at_ms = turn_started_at_ms - 60000 WHERE id = $1")
        .bind(id)
        .execute(&api.pool)
        .await
        .unwrap();
    let update = gr.expect("GAME_UPDATE").await;
    assert_eq!(
        (
            update["game"]["status"].as_str(),
            update["game"]["result"].as_str(),
            update["game"]["end_reason"].as_str()
        ),
        (Some("finished"), Some("black"), Some("timeout"))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn clock_syncs_arrive_with_the_current_sequence_number(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, _grace, id) = game(&api).await;
    let mut a = Client::hello(addr, &ada).await;
    let snap = a.subscribe(id).await;
    let sync = a.expect("CLOCK_SYNC").await;
    assert_eq!(
        (
            sync["game_id"].as_str().unwrap(),
            sync["turn"].as_str(),
            sync["event_seq"].clone()
        ),
        (
            id.to_string().as_str(),
            Some("white"),
            snap["event_seq"].clone()
        )
    );
    let (white, black) = (
        sync["clock"]["white_ms"].as_i64().unwrap(),
        sync["clock"]["black_ms"].as_i64().unwrap(),
    );
    assert!(
        white <= 300_000 && white > 290_000 && black == 300_000,
        "{white} {black}"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn unsubscribing_stops_the_updates(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let (mut a, mut g) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
    );
    a.subscribe(id).await;
    g.subscribe(id).await;
    a.send(json!({ "type": "UNSUBSCRIBE", "game_id": id }))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    api.post(
        &format!("/v1/games/{id}/moves"),
        &ada,
        json!({ "uci": "e2e4", "client_move_id": Uuid::now_v7() }),
    )
    .await;
    g.expect("GAME_UPDATE").await;
    assert!(
        a.quiet_for(500).await.is_empty(),
        "Ada stopped watching, so she hears nothing"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn signing_out_closes_the_live_connection(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, _grace, id) = game(&api).await;
    let mut a = Client::hello(addr, &ada).await;
    a.subscribe(id).await;
    assert_eq!(
        api.call(
            axum::http::Method::POST,
            "/v1/auth/logout",
            Some(&ada),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(a.expect("ERROR").await["code"], "unauthorized");
    assert!(a.is_closed_soon(1_000).await);
    assert_eq!(api.state.hub.connections(), 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_connection_that_stops_answering_pings_is_closed(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let ada = api.player("ada").await;
    let mut a = Client::hello(addr, &ada).await;
    a.auto_pong = false; // the app has frozen: it reads but never answers
    assert_eq!(a.expect("ERROR").await["code"], "timeout");
    assert!(a.is_closed_soon(1_000).await);
    assert_eq!(api.state.hub.connections(), 0, "and the server forgets it");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn misbehaving_clients_are_cut_off(pool: PgPool) {
    let api = Api::with_config(
        pool,
        Config {
            ws_max_subscriptions: 2,
            ws_max_messages_per_second: 30,
            ..fast()
        },
    );
    let addr = api.serve().await;
    let (ada, _grace, id) = game(&api).await;

    // Garbage: a few bad messages are tolerated and answered, then the connection is closed.
    let mut c = Client::hello(addr, &ada).await;
    for _ in 0..6 {
        c.send_text("{ nonsense".into()).await;
    }
    assert_eq!(c.expect("ERROR").await["code"], "bad_message");
    assert!(c.is_closed_soon(1_500).await);

    // Flooding.
    let mut c = Client::hello(addr, &ada).await;
    for _ in 0..60 {
        c.send(json!({ "type": "PING", "id": 1 })).await;
    }
    let mut saw_limit = false;
    while let Got::Msg(m) = c.next(Duration::from_secs(2)).await {
        saw_limit |= m["code"] == "rate_limited";
    }
    assert!(saw_limit, "told it is sending too much");

    // An enormous message.
    let mut c = Client::hello(addr, &ada).await;
    c.send_text("x".repeat(100_000)).await;
    assert!(c.is_closed_soon(1_500).await, "closed, not buffered");

    // Watching too many games at once.
    let mut c = Client::hello(addr, &ada).await;
    c.subscribe(id).await;
    for _ in 0..1 {
        let (_, g) = api
            .post(
                "/v1/games",
                &ada,
                json!({ "time_control": { "base_seconds": 60 } }),
            )
            .await;
        c.subscribe(id_of(&g)).await;
    }
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 60 } }),
        )
        .await;
    c.send(json!({ "type": "SUBSCRIBE", "game_id": id_of(&g) }))
        .await;
    assert_eq!(c.expect("ERROR").await["code"], "too_many_subscriptions");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_waiting_player_is_told_the_moment_matchmaking_finds_a_game(pool: PgPool) {
    // The waiting clients only answer pings while being read, so allow them a long silence.
    let config = Config {
        ws_pong_timeout_ms: 20_000,
        ..fast()
    };
    let one = Api::with_config(pool, config);
    let two = one.another_server();
    let (addr_one, addr_two) = (one.serve().await, two.serve().await);
    let (ada, grace, linus) = (
        one.player("ada").await,
        one.player("grace").await,
        one.player("linus").await,
    );
    let body = json!({ "time_control": { "base_seconds": 300 } });

    // Ada waits on server two; Linus is connected too but is not in the queue.
    let mut a = Client::hello(addr_two, &ada).await;
    let mut l = Client::hello(addr_one, &linus).await;
    assert_eq!(
        one.post("/v1/matchmaking", &ada, body.clone()).await.1["status"],
        "queued"
    );

    // Grace asks through server one; Ada, on server two, is told, with the id of the game.
    let (_, matched) = one.post("/v1/matchmaking", &grace, body.clone()).await;
    let game = matched["game"]["id"].as_str().unwrap().to_string();
    let found = a.expect("MATCH_FOUND").await;
    assert_eq!(found["game_id"], game.as_str());
    assert!(
        l.quiet_for(300).await.is_empty(),
        "a player who is not in the match hears nothing"
    );

    // What she is told is enough to start playing: subscribe and she has the game.
    let id: Uuid = game.parse().unwrap();
    assert_eq!(a.subscribe(id).await["game"]["status"], "active");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn both_players_are_told_when_the_background_sweep_pairs_them(pool: PgPool) {
    let config = Config {
        ws_pong_timeout_ms: 20_000,
        ..fast()
    };
    let api = Api::with_config(pool, config);
    let addr = api.serve().await;
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    for (name, rating) in [("ada", 1500.0), ("grace", 1900.0)] {
        sqlx::query("INSERT INTO ratings (user_id, category, rating, deviation, volatility) SELECT id, 'blitz', $2, 60, 0.06 FROM users WHERE username = $1")
            .bind(name).bind(rating).execute(&api.pool).await.unwrap();
    }
    let body = json!({ "time_control": { "base_seconds": 300 } });
    let (mut a, mut g) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
    );
    api.post("/v1/matchmaking", &ada, body.clone()).await;
    api.post("/v1/matchmaking", &grace, body).await;
    assert!(a.quiet_for(300).await.is_empty(), "too far apart for now");

    sqlx::query("UPDATE match_queue SET joined_at = joined_at - interval '20 seconds'")
        .execute(&api.pool)
        .await
        .unwrap();
    eschess_storage::matchmaking::sweep(&api.pool, &mut || true)
        .await
        .unwrap();
    let (fa, fg) = (a.expect("MATCH_FOUND").await, g.expect("MATCH_FOUND").await);
    assert_eq!(fa["game_id"], fg["game_id"]);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn draw_offers_and_their_answers_are_pushed_to_both_players(pool: PgPool) {
    let api = Api::with_config(pool, fast());
    let addr = api.serve().await;
    let (ada, grace, id) = game(&api).await;
    let (mut a, mut g) = (
        Client::hello(addr, &ada).await,
        Client::hello(addr, &grace).await,
    );
    a.subscribe(id).await;
    g.subscribe(id).await;

    api.post(&format!("/v1/games/{id}/draw-offer"), &ada, json!({}))
        .await;
    for c in [&mut a, &mut g] {
        assert_eq!(c.expect("GAME_UPDATE").await["game"]["draw_offer"], "white");
    }
    api.post(
        &format!("/v1/games/{id}/draw-offer/accept"),
        &grace,
        json!({}),
    )
    .await;
    let end = g.expect("GAME_UPDATE").await;
    assert_eq!(
        (
            end["game"]["result"].as_str(),
            end["game"]["end_reason"].as_str(),
            end["game"]["draw_offer"].is_null()
        ),
        (Some("draw"), Some("agreement"), true)
    );
}
