-- Glicko-2 ratings, one per player per speed class (bullet, blitz, rapid, classical).
--
-- `ratings` holds the current value; a player has no row in a class until their first rated game there (they are
-- treated as a newcomer: 1500, deviation 350). `rating_history` is append-only: one row per player per game, with the
-- numbers before and after, so every rating can be explained and recomputed. The primary key (game_id, user_id) also
-- means a game can never be counted twice.

CREATE TABLE ratings (
    user_id      UUID NOT NULL REFERENCES users (id),
    category     TEXT NOT NULL CHECK (category IN ('bullet', 'blitz', 'rapid', 'classical')),
    rating       DOUBLE PRECISION NOT NULL,
    deviation    DOUBLE PRECISION NOT NULL CHECK (deviation BETWEEN 30 AND 350),
    volatility   DOUBLE PRECISION NOT NULL CHECK (volatility > 0),
    games_played INTEGER NOT NULL DEFAULT 0 CHECK (games_played >= 0),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, category)
);
CREATE INDEX ratings_leaderboard ON ratings (category, rating DESC);

CREATE TABLE rating_history (
    game_id           UUID NOT NULL REFERENCES games (id),
    user_id           UUID NOT NULL REFERENCES users (id),
    category          TEXT NOT NULL,
    rating_before     DOUBLE PRECISION NOT NULL,
    deviation_before  DOUBLE PRECISION NOT NULL,
    volatility_before DOUBLE PRECISION NOT NULL,
    rating_after      DOUBLE PRECISION NOT NULL,
    deviation_after   DOUBLE PRECISION NOT NULL,
    volatility_after  DOUBLE PRECISION NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (game_id, user_id)
);
CREATE INDEX rating_history_user ON rating_history (user_id, created_at DESC);
SELECT eschess_make_append_only('rating_history');
