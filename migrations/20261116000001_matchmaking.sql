-- The matchmaking queue: players waiting to be paired with someone of similar strength.
--
-- One row per player at most (the primary key), so a player cannot be queued twice. While `game_id` is empty the player
-- is still waiting; once paired, both players' rows get the new game's id and stay for a while so that a player who was
-- not connected at that moment can still find out which game to go to.
CREATE TABLE match_queue (
    user_id      UUID PRIMARY KEY REFERENCES users (id),
    base_ms      BIGINT NOT NULL CHECK (base_ms BETWEEN 1000 AND 10800000),
    increment_ms BIGINT NOT NULL CHECK (increment_ms BETWEEN 0 AND 180000),
    rated        BOOLEAN NOT NULL,
    category     TEXT NOT NULL CHECK (category IN ('bullet', 'blitz', 'rapid', 'classical')),
    -- Their rating when they joined, which is what they are paired by.
    rating       DOUBLE PRECISION NOT NULL,
    joined_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    game_id      UUID REFERENCES games (id),
    matched_at   TIMESTAMPTZ,
    CHECK ((game_id IS NULL) = (matched_at IS NULL))
);
CREATE INDEX match_queue_waiting ON match_queue (base_ms, increment_ms, rated) WHERE game_id IS NULL;
