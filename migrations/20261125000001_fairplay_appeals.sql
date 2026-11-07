-- Appeals (ADR 0003 §5): the player appeals a sanction once, in writing; a reviewer who had no part in it upholds or
-- overturns it. Everything is append-only; overturning a sanction adds a decision, it never deletes the sanction.
CREATE TABLE fairplay_appeals (
    id          UUID PRIMARY KEY,
    sanction_id UUID NOT NULL UNIQUE REFERENCES fairplay_sanctions (id),
    player_id   UUID NOT NULL REFERENCES users (id),
    statement   TEXT NOT NULL CHECK (length(btrim(statement)) BETWEEN 1 AND 4000),
    filed_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE FUNCTION fairplay_appeals_guard() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM fairplay_sanctions WHERE id = NEW.sanction_id AND player_id = NEW.player_id AND kind <> 'warning') THEN
        RAISE EXCEPTION 'an appeal is by the sanctioned player, against a suspension or closure' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_appeals_guard BEFORE INSERT ON fairplay_appeals
    FOR EACH ROW EXECUTE FUNCTION fairplay_appeals_guard();

CREATE TABLE fairplay_appeal_decisions (
    appeal_id   UUID PRIMARY KEY REFERENCES fairplay_appeals (id),
    reviewer_id UUID NOT NULL REFERENCES users (id),
    decision    TEXT NOT NULL CHECK (decision IN ('upheld', 'overturned')),
    reason      TEXT NOT NULL CHECK (length(btrim(reason)) > 0),
    decided_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The appeal is decided by someone who had no part in the sanction: not the player, not whoever confirmed the case or
-- reviewed its closure, and nobody who played in one of its games.
CREATE FUNCTION fairplay_appeal_decisions_guard() RETURNS trigger AS $$
DECLARE s fairplay_sanctions; c fairplay_cases;
BEGIN
    SELECT z.* INTO s FROM fairplay_sanctions z JOIN fairplay_appeals a ON a.sanction_id = z.id WHERE a.id = NEW.appeal_id;
    SELECT * INTO c FROM fairplay_cases WHERE id = s.case_id;
    IF NEW.reviewer_id IN (s.player_id, c.decided_by)
       OR EXISTS (SELECT 1 FROM fairplay_closure_reviews r WHERE r.sanction_id = s.id AND r.reviewer_id = NEW.reviewer_id)
       OR EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id
                   WHERE cg.case_id = c.id AND NEW.reviewer_id IN (g.white_id, g.black_id)) THEN
        RAISE EXCEPTION 'this reviewer had a part in the sanction and cannot decide its appeal' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_appeal_decisions_guard BEFORE INSERT ON fairplay_appeal_decisions
    FOR EACH ROW EXECUTE FUNCTION fairplay_appeal_decisions_guard();

CREATE TRIGGER fairplay_appeals_no_change BEFORE UPDATE OR DELETE OR TRUNCATE ON fairplay_appeals
    FOR EACH STATEMENT EXECUTE FUNCTION fairplay_sanctions_immutable();
CREATE TRIGGER fairplay_appeal_decisions_no_change BEFORE UPDATE OR DELETE OR TRUNCATE ON fairplay_appeal_decisions
    FOR EACH STATEMENT EXECUTE FUNCTION fairplay_sanctions_immutable();

-- What restricts a player right now: a suspension that has not ended, or a closure a second reviewer approved, unless
-- an appeal overturned it. The one place that says so, so no query can forget the appeal.
CREATE VIEW fairplay_restrictions AS
SELECT z.id AS sanction_id, z.player_id, z.kind, z.ends_at
  FROM fairplay_sanctions z
 WHERE NOT EXISTS (SELECT 1 FROM fairplay_appeals a JOIN fairplay_appeal_decisions d ON d.appeal_id = a.id
                    WHERE a.sanction_id = z.id AND d.decision = 'overturned')
   AND ((z.kind = 'suspension' AND z.ends_at > now())
     OR (z.kind = 'closure' AND EXISTS (SELECT 1 FROM fairplay_closure_reviews r
                                         WHERE r.sanction_id = z.id AND r.decision = 'approved')));
