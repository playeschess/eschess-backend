-- Fair play (ADR 0002 §3): two more facts about each move, for scoring. Both nullable, so moves recorded before this
-- migration stay valid (an expand-only change).
--
--   transport        'ws' or 'http': how the move reached the server.
--   lag_measured_ms  the round-trip lag the server measured for the mover's connection when the move arrived. Only the
--                    WebSocket can measure it, so it is NULL for HTTP moves. Until now only the refund (credited_ms) was
--                    kept, which hides what the connection was really doing.
--
-- game_moves is append-only; adding columns does not change any existing row.
ALTER TABLE game_moves
    ADD COLUMN transport       TEXT CHECK (transport IN ('ws', 'http')),
    ADD COLUMN lag_measured_ms BIGINT CHECK (lag_measured_ms >= 0),
    -- A measured lag only exists for a WebSocket move.
    ADD CONSTRAINT game_moves_lag_only_over_ws CHECK (lag_measured_ms IS NULL OR transport = 'ws');
