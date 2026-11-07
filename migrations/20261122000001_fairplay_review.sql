-- Fair-play review (ADR 0002 §5): the reviewer role, and the rule that nobody judges a game they played in.

-- A new role, granted by hand (SQL) like withdrawal_approver.
ALTER TABLE user_roles DROP CONSTRAINT user_roles_role_check;
ALTER TABLE user_roles ADD CONSTRAINT user_roles_role_check
    CHECK (role IN ('withdrawal_approver', 'fairplay_reviewer'));

-- The case guard gains one rule: whoever decides a case did not play in any of its games. The player the case is about
-- is already excluded by a CHECK; this also excludes their opponents, who have a stake in the outcome.
CREATE OR REPLACE FUNCTION fairplay_cases_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.id, NEW.player_id, NEW.opened_at) IS DISTINCT FROM (OLD.id, OLD.player_id, OLD.opened_at) THEN
        RAISE EXCEPTION 'fairplay case %: who it is about and when it opened cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.status <> 'open' THEN
        RAISE EXCEPTION 'fairplay case % is decided (%) and cannot change', OLD.id, OLD.status USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.decided_by IS NOT NULL AND EXISTS (
        SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id
         WHERE cg.case_id = OLD.id AND NEW.decided_by IN (g.white_id, g.black_id)
    ) THEN
        RAISE EXCEPTION 'fairplay case %: % played in one of its games and cannot decide it', OLD.id, NEW.decided_by
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
