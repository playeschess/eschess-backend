-- Staked games played on Lichess (ADR 0005, build step 3).
--
-- Where new staked games are played is a setting every server and worker reads from the database, like `stake_path`. It
-- starts as `own`; flipping it to `lichess` is a deliberate act, done only once the fair-play data of Lichess games is
-- imported (build step 4). The game remembers its venue from the moment it starts.
INSERT INTO platform_settings (name, value) VALUES ('game_venue', 'own');

-- When the Lichess challenge disappears if nobody has started the game. A game still unstarted after this is a no-show.
ALTER TABLE games
    ADD COLUMN lichess_challenge_expires_at TIMESTAMPTZ,
    ADD CONSTRAINT games_lichess_is_staked CHECK (venue = 'own' OR stake_minor IS NOT NULL);

-- The venue and the Lichess id may now change in three ways, each once:
--   * a waiting game becomes a Lichess game as it starts (no Lichess id yet),
--   * the id is filled in when the Lichess challenge has been made (a Lichess game with no id, no move yet),
--   * a game with no move yet is attached to a Lichess game in one step (`attach_lichess_game`).
-- Nothing else, and never back.
CREATE OR REPLACE FUNCTION games_venue_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.venue, NEW.lichess_game_id) IS DISTINCT FROM (OLD.venue, OLD.lichess_game_id) THEN
        IF NOT (
            NEW.venue = 'lichess' AND OLD.ply = 0 AND (
                (OLD.venue = 'own' AND OLD.status = 'waiting' AND NEW.lichess_game_id IS NULL)
                OR (OLD.venue = 'lichess' AND OLD.lichess_game_id IS NULL AND NEW.lichess_game_id IS NOT NULL
                    AND OLD.status IN ('funding', 'active'))
                OR (OLD.venue = 'own' AND NEW.lichess_game_id IS NOT NULL AND OLD.status IN ('funding', 'active'))
            )
        ) THEN
            RAISE EXCEPTION 'game %: the venue cannot change', OLD.id USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
