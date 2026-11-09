-- Staked matchmaking (ADR 0004 step 1). A queued player may ask for a stake; only players asking for the same stake can
-- meet. Nothing is held while queued: the stakes are held in the transaction that pairs two players, so there is nothing
-- to give back when someone leaves, times out, or is closed.
ALTER TABLE match_queue
    ADD COLUMN stake_minor BIGINT CHECK (stake_minor > 0),
    ADD COLUMN stake_asset TEXT CHECK (length(stake_asset) > 0),
    ADD CONSTRAINT match_queue_stake_has_asset CHECK ((stake_minor IS NULL) = (stake_asset IS NULL)),
    ADD CONSTRAINT match_queue_stake_is_rated CHECK (stake_minor IS NULL OR rated);
