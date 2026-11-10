-- Staked games on the escrow contract (ADR 0007, build step 1).
--
-- A staked game made on the contract path starts in a new state, `funding`: its players are known and the ledger already
-- holds both stakes, but no clock runs until a worker job has locked the stakes in the contract on the network. The game
-- then becomes `active`. A game that cannot be funded in time is cancelled and the ledger holds are released.

-- Where stakes live is a setting every server and worker reads from the database, so they cannot disagree. It is read when
-- a staked game is made, and the game remembers it, so changing the setting never strands a game halfway.
CREATE TABLE platform_settings (
    name       TEXT PRIMARY KEY CHECK (length(btrim(name)) > 0),
    value      TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO platform_settings (name, value) VALUES ('stake_path', 'ledger');

ALTER TABLE games DROP CONSTRAINT games_status_check;
ALTER TABLE games ADD CONSTRAINT games_status_check
    CHECK (status IN ('waiting', 'funding', 'active', 'finished', 'cancelled'));
ALTER TABLE games
    ADD COLUMN stake_path TEXT NOT NULL DEFAULT 'ledger' CHECK (stake_path IN ('ledger', 'contract')),
    ADD COLUMN funding_started_at TIMESTAMPTZ,
    ADD CONSTRAINT games_contract_path_is_staked CHECK (stake_path = 'ledger' OR stake_minor IS NOT NULL),
    ADD CONSTRAINT games_funding_has_players CHECK (status <> 'funding' OR (white_id IS NOT NULL AND black_id IS NOT NULL AND stake_path = 'contract'));

-- The stake and now the path it lives on cannot change after the game is made.
CREATE OR REPLACE FUNCTION games_stake_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.stake_minor, NEW.stake_asset, NEW.stake_path) IS DISTINCT FROM (OLD.stake_minor, OLD.stake_asset, OLD.stake_path) THEN
        RAISE EXCEPTION 'game %: the stake cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.settled_at IS NOT NULL AND NEW.settled_at IS DISTINCT FROM OLD.settled_at THEN
        RAISE EXCEPTION 'game % is settled and cannot be settled again', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Every transaction sent to the network for a staked game, recorded BEFORE it is sent. A transaction that may still land
-- is waited for; a new one is built only once the network is past its expiry (ADR 0001's rule).
CREATE TABLE contract_txs (
    tx_hash    TEXT PRIMARY KEY CHECK (tx_hash ~ '^[0-9a-f]{64}$'),
    game_id    UUID NOT NULL REFERENCES games (id),
    step       TEXT NOT NULL CHECK (step IN ('create', 'top_up', 'deposit', 'cancel', 'settle')),
    -- Whose action it was, for deposits and top-ups.
    player_id  UUID REFERENCES users (id),
    max_time   BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX contract_txs_game ON contract_txs (game_id, created_at DESC);
SELECT eschess_make_append_only('contract_txs');
