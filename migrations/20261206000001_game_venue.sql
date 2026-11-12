-- Where a game is played (ADR 0005, direction chosen 2026-11-12). `own` is our game server: we run the rules and the
-- clock and we record the result. `lichess` is a game played on lichess.org: the result is read from Lichess, and our
-- server must not run a clock or accept moves for it.
--
-- Settlement does not care which: it reads the result and ply count from this table once the game is `finished`. So a
-- result from Lichess is simply written here, once, by `games::finish_external`.

ALTER TABLE games
    ADD COLUMN venue TEXT NOT NULL DEFAULT 'own' CHECK (venue IN ('own', 'lichess')),
    ADD COLUMN lichess_game_id TEXT,
    ADD CONSTRAINT games_lichess_id_only_on_lichess CHECK (venue = 'lichess' OR lichess_game_id IS NULL),
    ADD CONSTRAINT games_lichess_id_shape CHECK (lichess_game_id IS NULL OR lichess_game_id ~ '^[A-Za-z0-9]{8}$');

-- One Lichess game belongs to at most one of our games, so one result can never settle two pots.
CREATE UNIQUE INDEX games_lichess_game_id ON games (lichess_game_id) WHERE lichess_game_id IS NOT NULL;

-- The venue changes at most once: `own` to `lichess`, when the Lichess game has been created, and only before a move
-- has been played on our server. After that neither it nor the Lichess id can change.
CREATE OR REPLACE FUNCTION games_venue_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.venue, NEW.lichess_game_id) IS DISTINCT FROM (OLD.venue, OLD.lichess_game_id) THEN
        IF OLD.venue <> 'own' OR NEW.venue <> 'lichess' OR NEW.lichess_game_id IS NULL
           OR OLD.status NOT IN ('funding', 'active') OR OLD.ply <> 0 THEN
            RAISE EXCEPTION 'game %: the venue cannot change', OLD.id USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER games_venue_guard BEFORE UPDATE ON games
    FOR EACH ROW EXECUTE FUNCTION games_venue_guard();
