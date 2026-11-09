-- Staked games (ADR 0004, build step 1).
--
-- A staked game is a rated game with one stake: the same amount of one asset from each player. The stakes are held in
-- the ledger (user_wallet -> stake_escrow) when the game is created and joined; this migration only records what the
-- stake is. Money never moves from these columns, only from ledger postings.
--
--   stake_minor  the stake each player puts in, in the asset's minor units; NULL for an unstaked game
--   stake_asset  the asset ('CODE:ISSUER'); NULL exactly when the game is unstaked
--   settled_at   set by the settle job (step 2) once the pot has been paid or returned; nothing sets it yet
--
-- Rules the database keeps: only rated games are staked, a stake has an asset and an asset has a stake, and neither
-- can change after the game is created, so what was held is always what is released.
ALTER TABLE games
    ADD COLUMN stake_minor BIGINT CHECK (stake_minor > 0),
    ADD COLUMN stake_asset TEXT CHECK (length(stake_asset) > 0),
    ADD COLUMN settled_at  TIMESTAMPTZ,
    ADD CONSTRAINT games_stake_has_asset CHECK ((stake_minor IS NULL) = (stake_asset IS NULL)),
    ADD CONSTRAINT games_stake_is_rated CHECK (stake_minor IS NULL OR rated),
    ADD CONSTRAINT games_settled_is_staked CHECK (settled_at IS NULL OR stake_minor IS NOT NULL);

CREATE FUNCTION games_stake_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.stake_minor, NEW.stake_asset) IS DISTINCT FROM (OLD.stake_minor, OLD.stake_asset) THEN
        RAISE EXCEPTION 'game %: the stake cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.settled_at IS NOT NULL AND NEW.settled_at IS DISTINCT FROM OLD.settled_at THEN
        RAISE EXCEPTION 'game % is settled and cannot be settled again', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER games_stake_guard BEFORE UPDATE ON games
    FOR EACH ROW EXECUTE FUNCTION games_stake_guard();

-- Staked games waiting for settlement are found by the worker (step 2) through this.
CREATE INDEX games_unsettled ON games (ended_at) WHERE stake_minor IS NOT NULL AND settled_at IS NULL AND status = 'finished';
