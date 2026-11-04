//! Who is connected and watching which game, and how a change in one server reaches players on another.
//!
//! The hub holds only connection handles (nothing a game depends on): losing a server loses the connections and
//! nothing else, because the truth is in Postgres and players simply reconnect and ask for a snapshot.
//!
//! Servers learn about each other's changes through Postgres itself: every change to a game sends a `NOTIFY` on the
//! `game_events` channel (delivered only if the change commits), and each server `LISTEN`s and pushes the new state to
//! its own connected players. That is why no extra message broker is needed yet.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eschess_storage::games;
use serde_json::json;
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::routes::games::game_json;

/// The queue of messages waiting to be written to one connection. Bounded: a client that cannot keep up is dropped
/// rather than allowed to make the server hold ever more memory for it.
pub type Outbox = mpsc::Sender<String>;

#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    conns: HashMap<u64, Conn>,
    by_game: HashMap<Uuid, HashSet<u64>>,
}

struct Conn {
    user: Uuid,
    outbox: Outbox,
    games: HashSet<Uuid>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubscribeError {
    TooMany,
    Gone,
}

impl Hub {
    pub fn new() -> Hub {
        Hub::default()
    }

    pub fn register(&self, user: Uuid, outbox: Outbox) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.conns.insert(
            id,
            Conn {
                user,
                outbox,
                games: HashSet::new(),
            },
        );
        id
    }

    pub fn remove(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(conn) = inner.conns.remove(&id) {
            for game in conn.games {
                if let Some(set) = inner.by_game.get_mut(&game) {
                    set.remove(&id);
                    if set.is_empty() {
                        inner.by_game.remove(&game);
                    }
                }
            }
        }
    }

    pub fn is_registered(&self, id: u64) -> bool {
        self.inner.lock().unwrap().conns.contains_key(&id)
    }

    pub fn connections(&self) -> usize {
        self.inner.lock().unwrap().conns.len()
    }

    pub fn subscribe(&self, id: u64, game: Uuid, max: usize) -> Result<(), SubscribeError> {
        let mut inner = self.inner.lock().unwrap();
        let conn = inner.conns.get_mut(&id).ok_or(SubscribeError::Gone)?;
        if !conn.games.contains(&game) && conn.games.len() >= max {
            return Err(SubscribeError::TooMany);
        }
        conn.games.insert(game);
        inner.by_game.entry(game).or_default().insert(id);
        Ok(())
    }

    pub fn unsubscribe(&self, id: u64, game: Uuid) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(conn) = inner.conns.get_mut(&id) {
            conn.games.remove(&game);
        }
        if let Some(set) = inner.by_game.get_mut(&game) {
            set.remove(&id);
            if set.is_empty() {
                inner.by_game.remove(&game);
            }
        }
    }

    pub fn subscribed_games(&self, id: u64) -> Vec<Uuid> {
        self.inner
            .lock()
            .unwrap()
            .conns
            .get(&id)
            .map(|c| c.games.iter().copied().collect())
            .unwrap_or_default()
    }

    /// (connection, player) for everyone watching `game`.
    pub fn watchers(&self, game: Uuid) -> Vec<(u64, Uuid)> {
        let inner = self.inner.lock().unwrap();
        inner
            .by_game
            .get(&game)
            .into_iter()
            .flatten()
            .filter_map(|id| inner.conns.get(id).map(|c| (*id, c.user)))
            .collect()
    }

    /// Every (game, connection, player) being watched, for refreshing everyone after a missed notification.
    fn everything(&self) -> Vec<(Uuid, u64, Uuid)> {
        let inner = self.inner.lock().unwrap();
        inner
            .by_game
            .iter()
            .flat_map(|(game, ids)| {
                ids.iter()
                    .filter_map(|id| inner.conns.get(id).map(|c| (*game, *id, c.user)))
            })
            .collect()
    }

    /// Queues a message for a connection. A connection whose queue is full cannot keep up and is dropped.
    pub fn send(&self, id: u64, message: String) {
        let outbox = self
            .inner
            .lock()
            .unwrap()
            .conns
            .get(&id)
            .map(|c| c.outbox.clone());
        if let Some(outbox) = outbox {
            if outbox.try_send(message).is_err() {
                tracing::warn!("connection {id} is not keeping up with its messages; dropping it");
                self.remove(id);
            }
        }
    }
}

/// The message that carries a game's new state to a watcher.
pub fn update_message(game: &games::GameView) -> String {
    json!({ "type": "GAME_UPDATE", "event_seq": game.event_seq, "game": game_json(game) })
        .to_string()
}

async fn push_game(hub: &Hub, pool: &PgPool, game: Uuid) {
    for (conn, user) in hub.watchers(game) {
        push_to(hub, pool, game, conn, user).await;
    }
}

async fn push_to(hub: &Hub, pool: &PgPool, game: Uuid, conn: u64, user: Uuid) {
    match games::peek_game(pool, game, user).await {
        Ok(view) => hub.send(conn, update_message(&view)),
        // The game is no longer something this player may see; say nothing.
        Err(games::GameError::NotFound) => {}
        Err(e) => tracing::error!("could not read game {game} for a live update: {e}"),
    }
}

/// Starts this server's side of the live-update system: listens for game changes announced by any server (including
/// this one) and pushes the new state to everyone connected here who is watching that game.
pub fn spawn_listener(hub: Hub, pool: PgPool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let mut listener = match PgListener::connect_with(&pool).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("cannot start listening for game changes: {e}; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if let Err(e) = listener.listen("game_events").await {
                tracing::warn!("cannot listen on game_events: {e}; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            // Anything may have been missed while not listening: bring every watcher up to date.
            for (game, conn, user) in hub.everything() {
                push_to(&hub, &pool, game, conn, user).await;
            }
            loop {
                match listener.recv().await {
                    Ok(notification) => {
                        if let Ok(game) = notification.payload().parse::<Uuid>() {
                            push_game(&hub, &pool, game).await;
                        }
                    }
                    Err(e) => {
                        // The listener reconnects by itself on the next call; notifications sent meanwhile are lost,
                        // so refresh everyone (clients also notice a gap in `event_seq` on their next clock sync).
                        tracing::warn!("lost the game_events connection: {e}");
                        for (game, conn, user) in hub.everything() {
                            push_to(&hub, &pool, game, conn, user).await;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox() -> (Outbox, mpsc::Receiver<String>) {
        mpsc::channel(2)
    }

    #[test]
    fn watchers_come_and_go_and_limits_hold() {
        let hub = Hub::new();
        let (a_tx, _a_rx) = outbox();
        let (b_tx, _b_rx) = outbox();
        let (ada, grace, game) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(9));
        let (a, b) = (hub.register(ada, a_tx), hub.register(grace, b_tx));
        assert_eq!(hub.connections(), 2);
        hub.subscribe(a, game, 2).unwrap();
        hub.subscribe(b, game, 2).unwrap();
        let mut who: Vec<Uuid> = hub.watchers(game).into_iter().map(|(_, u)| u).collect();
        who.sort();
        assert_eq!(who, vec![ada, grace]);

        // A connection may watch only so many games; watching the same one again is free.
        hub.subscribe(a, Uuid::from_u128(10), 2).unwrap();
        assert_eq!(
            hub.subscribe(a, Uuid::from_u128(11), 2),
            Err(SubscribeError::TooMany)
        );
        hub.subscribe(a, game, 2).unwrap();

        hub.unsubscribe(b, game);
        assert_eq!(hub.watchers(game).len(), 1);
        hub.remove(a);
        assert!(hub.watchers(game).is_empty() && !hub.is_registered(a) && hub.is_registered(b));
        assert_eq!(hub.subscribe(a, game, 2), Err(SubscribeError::Gone));
    }

    #[tokio::test]
    async fn a_connection_that_cannot_keep_up_is_dropped() {
        let hub = Hub::new();
        let (tx, mut rx) = mpsc::channel(2);
        let id = hub.register(Uuid::from_u128(1), tx);
        hub.send(id, "one".into());
        hub.send(id, "two".into());
        assert!(hub.is_registered(id));
        hub.send(id, "three".into()); // the queue holds two: this one does not fit
        assert!(
            !hub.is_registered(id),
            "dropped rather than buffered without limit"
        );
        assert_eq!(rx.recv().await.as_deref(), Some("one"));
    }
}
