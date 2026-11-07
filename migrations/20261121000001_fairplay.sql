-- Fair play scores and the review queue (ADR 0002 §4).
--
-- A scorer looks at finished games. For each (game, scorer) there is exactly one run: the game was scored, or it could
-- not be (and why). A run makes scoring idempotent: a game is looked at once per scorer version, however many workers
-- run, and a new version scores everything again without touching the old scores.
--
-- A score at or above the case threshold (configuration of whichever worker runs the scorer) opens a case for the
-- player, or adds the game to their open case. A case is open until a reviewer clears or confirms it.
--
-- Scores are never a verdict. In this baseline a decision is recorded and does nothing else (ADR 0002 §6).

CREATE TABLE fairplay_runs (
    game_id   UUID NOT NULL REFERENCES games (id),
    scorer    TEXT NOT NULL CHECK (scorer ~ '^[a-z0-9._-]{1,64}$'),
    -- scored      the scorer ran and its scores (possibly none) are stored
    -- unscorable  it could not be scored: the stored game is inconsistent, the scorer failed, or it returned scores
    --             that broke the rules. Kept for a person to look at, never retried by the same scorer.
    outcome   TEXT NOT NULL CHECK (outcome IN ('scored', 'unscorable')),
    reason    TEXT,
    ran_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (game_id, scorer),
    CHECK ((outcome = 'unscorable') = (reason IS NOT NULL))
);
CREATE INDEX fairplay_runs_unscorable ON fairplay_runs (ran_at) WHERE outcome = 'unscorable';
SELECT eschess_make_append_only('fairplay_runs');

-- Finding games a scorer has not looked at yet goes through finished games in the order they ended.
CREATE INDEX games_finished ON games (ended_at, id) WHERE status = 'finished';

CREATE TABLE fairplay_scores (
    game_id    UUID NOT NULL,
    player_id  UUID NOT NULL REFERENCES users (id),
    scorer     TEXT NOT NULL,
    score      SMALLINT NOT NULL CHECK (score BETWEEN 0 AND 100),
    summary    TEXT NOT NULL CHECK (length(summary) <= 2000),
    evidence   JSONB NOT NULL DEFAULT 'null',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (game_id, player_id, scorer),
    FOREIGN KEY (game_id, scorer) REFERENCES fairplay_runs (game_id, scorer)
);
SELECT eschess_make_append_only('fairplay_scores');

-- A score is only ever about someone who played the game, and only for a run that scored.
CREATE FUNCTION fairplay_scores_check() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM games WHERE id = NEW.game_id AND NEW.player_id IN (white_id, black_id)) THEN
        RAISE EXCEPTION 'fairplay score: % did not play game %', NEW.player_id, NEW.game_id USING ERRCODE = 'check_violation';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM fairplay_runs WHERE game_id = NEW.game_id AND scorer = NEW.scorer AND outcome = 'scored') THEN
        RAISE EXCEPTION 'fairplay score: game % was not scored by %', NEW.game_id, NEW.scorer USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_scores_check BEFORE INSERT ON fairplay_scores
    FOR EACH ROW EXECUTE FUNCTION fairplay_scores_check();

-- open       waiting for a reviewer
-- cleared    a reviewer found nothing wrong          (final)
-- confirmed  a reviewer found unfair play             (final)
CREATE TABLE fairplay_cases (
    id         UUID PRIMARY KEY,
    player_id  UUID NOT NULL REFERENCES users (id),
    status     TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'cleared', 'confirmed')),
    opened_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    decided_by UUID REFERENCES users (id),
    decided_at TIMESTAMPTZ,
    reason     TEXT CHECK (length(btrim(reason)) > 0),
    CHECK ((status = 'open') = (decided_by IS NULL)),
    CHECK ((status = 'open') = (decided_at IS NULL)),
    CHECK ((status = 'open') = (reason IS NULL)),
    -- Nobody decides a case about themselves.
    CHECK (decided_by IS NULL OR decided_by <> player_id)
);
-- One open case per player: new suspicious games join it rather than opening another.
CREATE UNIQUE INDEX fairplay_one_open_case ON fairplay_cases (player_id) WHERE status = 'open';
CREATE INDEX fairplay_open_cases ON fairplay_cases (opened_at, id) WHERE status = 'open';

CREATE FUNCTION fairplay_cases_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.id, NEW.player_id, NEW.opened_at) IS DISTINCT FROM (OLD.id, OLD.player_id, OLD.opened_at) THEN
        RAISE EXCEPTION 'fairplay case %: who it is about and when it opened cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.status <> 'open' THEN
        RAISE EXCEPTION 'fairplay case % is decided (%) and cannot change', OLD.id, OLD.status USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_cases_guard BEFORE UPDATE ON fairplay_cases
    FOR EACH ROW EXECUTE FUNCTION fairplay_cases_guard();
CREATE TRIGGER fairplay_cases_no_delete BEFORE DELETE OR TRUNCATE ON fairplay_cases
    FOR EACH STATEMENT EXECUTE FUNCTION eschess_refuse_change();

-- The games (and the scores that put them there) in a case.
CREATE TABLE fairplay_case_games (
    case_id   UUID NOT NULL REFERENCES fairplay_cases (id),
    game_id   UUID NOT NULL,
    player_id UUID NOT NULL,
    scorer    TEXT NOT NULL,
    added_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (case_id, game_id, scorer),
    FOREIGN KEY (game_id, player_id, scorer) REFERENCES fairplay_scores (game_id, player_id, scorer)
);
SELECT eschess_make_append_only('fairplay_case_games');

-- A game joins only an open case, and only a case about the player the score is for.
CREATE FUNCTION fairplay_case_games_check() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM fairplay_cases WHERE id = NEW.case_id AND status = 'open' AND player_id = NEW.player_id) THEN
        RAISE EXCEPTION 'fairplay case %: not an open case about %', NEW.case_id, NEW.player_id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_case_games_check BEFORE INSERT ON fairplay_case_games
    FOR EACH ROW EXECUTE FUNCTION fairplay_case_games_check();
