-- Account closure (ADR 0003 §3): a reviewer proposes it by confirming a case with a closure sanction, and a second
-- reviewer approves or declines it. It takes effect only when approved.
ALTER TABLE fairplay_sanctions DROP CONSTRAINT fairplay_sanctions_kind_check;
ALTER TABLE fairplay_sanctions ADD CONSTRAINT fairplay_sanctions_kind_check
    CHECK (kind IN ('warning', 'suspension', 'closure'));

CREATE TABLE fairplay_closure_reviews (
    sanction_id UUID PRIMARY KEY REFERENCES fairplay_sanctions (id),
    reviewer_id UUID NOT NULL REFERENCES users (id),
    decision    TEXT NOT NULL CHECK (decision IN ('approved', 'declined')),
    reason      TEXT NOT NULL CHECK (length(btrim(reason)) > 0),
    decided_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Only closures are reviewed, and not by the person it is about, by the reviewer who proposed it, or by anyone who
-- played in one of the case's games.
CREATE FUNCTION fairplay_closure_reviews_guard() RETURNS trigger AS $$
DECLARE s fairplay_sanctions; c fairplay_cases;
BEGIN
    SELECT * INTO s FROM fairplay_sanctions WHERE id = NEW.sanction_id;
    SELECT * INTO c FROM fairplay_cases WHERE id = s.case_id;
    IF s.kind <> 'closure' THEN
        RAISE EXCEPTION 'only a closure is reviewed' USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.reviewer_id IN (s.player_id, c.decided_by) THEN
        RAISE EXCEPTION 'a closure is reviewed by a second reviewer, not the player or the proposer' USING ERRCODE = 'check_violation';
    END IF;
    IF EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id
                WHERE cg.case_id = c.id AND NEW.reviewer_id IN (g.white_id, g.black_id)) THEN
        RAISE EXCEPTION 'a player in the case cannot review its closure' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_closure_reviews_guard BEFORE INSERT ON fairplay_closure_reviews
    FOR EACH ROW EXECUTE FUNCTION fairplay_closure_reviews_guard();
CREATE TRIGGER fairplay_closure_reviews_no_change BEFORE UPDATE OR DELETE OR TRUNCATE ON fairplay_closure_reviews
    FOR EACH STATEMENT EXECUTE FUNCTION fairplay_sanctions_immutable();
