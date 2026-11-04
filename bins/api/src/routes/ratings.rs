use axum::extract::{Query, State};
use axum::Json;
use eschess_storage::ratings;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

pub(crate) fn rating_json(r: &ratings::PlayerRating) -> Value {
    json!({
        "category": r.category,
        "rating": r.rating.round() as i64,
        "deviation": r.deviation.round() as i64,
        "games_played": r.games_played,
        // Until the deviation is small the number is a rough guess, and clients should show it as provisional.
        "provisional": r.deviation > ratings::ESTABLISHED_DEVIATION,
    })
}

#[derive(Deserialize)]
pub struct BoardQuery {
    category: Option<String>,
    limit: Option<i64>,
}

/// The best players in a speed class: `?category=blitz` (the default), `?limit=` up to 100.
pub async fn leaderboard(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(q): Query<BoardQuery>,
) -> Result<Json<Value>, ApiError> {
    let category = q.category.as_deref().unwrap_or("blitz");
    if !["bullet", "blitz", "rapid", "classical"].contains(&category) {
        return Err(ApiError::Validation(
            "category must be bullet, blitz, rapid or classical".into(),
        ));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let rows = ratings::leaderboard(&state.pool, category, limit)
        .await
        .map_err(ApiError::internal)?;
    let players: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(i, p)| json!({ "rank": i + 1, "username": p.username, "rating": p.rating.round() as i64, "games_played": p.games_played }))
        .collect();
    Ok(Json(json!({ "category": category, "players": players })))
}
