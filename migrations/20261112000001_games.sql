-- Games and their moves. The database is the authority on what happened in a game: the current position, whose turn
-- it is and how much time everyone has all live here, so any server can pick a game up after a restart.

CREATE TABLE games (
    id                 UUID PRIMARY KEY,
    created_by         UUID NOT NULL REFERENCES users (id),
    color_preference   TEXT NOT NULL CHECK (color_preference IN ('white', 'black', 'random')),
    -- Colours are assigned when the second player joins, so both are empty while the game is waiting.
    white_id           UUID REFERENCES users (id),
    black_id           UUID REFERENCES users (id),
    base_ms            BIGINT NOT NULL CHECK (base_ms BETWEEN 1000 AND 10800000),
    increment_ms       BIGINT NOT NULL CHECK (increment_ms BETWEEN 0 AND 180000),
    status             TEXT NOT NULL DEFAULT 'waiting' CHECK (status IN ('waiting', 'active', 'finished', 'cancelled')),
    result             TEXT CHECK (result IN ('white', 'black', 'draw')),
    end_reason         TEXT,
    fen                TEXT NOT NULL,
    ply                INTEGER NOT NULL DEFAULT 0 CHECK (ply >= 0),
    -- The clock, as the chess clock module stores it: time banked when the current turn began, and when it began.
    white_remaining_ms BIGINT NOT NULL CHECK (white_remaining_ms >= 0),
    black_remaining_ms BIGINT NOT NULL CHECK (black_remaining_ms >= 0),
    white_lag_quota_ms BIGINT NOT NULL CHECK (white_lag_quota_ms >= 0),
    black_lag_quota_ms BIGINT NOT NULL CHECK (black_lag_quota_ms >= 0),
    turn_started_at_ms BIGINT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at         TIMESTAMPTZ,
    ended_at           TIMESTAMPTZ,
    CHECK (white_id IS NULL OR black_id IS NULL OR white_id <> black_id),
    CHECK (status NOT IN ('active', 'finished') OR (white_id IS NOT NULL AND black_id IS NOT NULL)),
    CHECK ((status = 'finished') = (result IS NOT NULL)),
    CHECK (status <> 'active' OR turn_started_at_ms IS NOT NULL)
);
CREATE INDEX games_open ON games (created_at DESC) WHERE status = 'waiting';
CREATE INDEX games_white ON games (white_id, status);
CREATE INDEX games_black ON games (black_id, status);
CREATE INDEX games_creator ON games (created_by, status);

-- Every move ever played. Append-only: a game's history cannot be rewritten. `client_move_id` is chosen by the player's
-- app and sent with the move; if the same id arrives again (a retry on a bad connection) the original outcome is replayed
-- instead of the move being played twice.
CREATE TABLE game_moves (
    game_id            UUID NOT NULL REFERENCES games (id),
    ply                INTEGER NOT NULL CHECK (ply >= 1),
    uci                TEXT NOT NULL,
    fen_after          TEXT NOT NULL,
    mover_id           UUID NOT NULL REFERENCES users (id),
    client_move_id     UUID NOT NULL,
    charged_ms         BIGINT NOT NULL,
    credited_ms        BIGINT NOT NULL,
    remaining_ms_after BIGINT NOT NULL,
    played_at_ms       BIGINT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (game_id, ply),
    UNIQUE (game_id, client_move_id)
);

SELECT eschess_make_append_only('game_moves');
