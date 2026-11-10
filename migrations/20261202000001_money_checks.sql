-- The books-against-chain check (ADR 0006 §6). Money is now spread over many accounts on the network, so the platform
-- regularly compares what the ledger says it holds with what the network says is there. Every check is kept; a mismatch
-- that persists halts new outflows until a person looks. Nothing here repairs anything.
CREATE TABLE money_checks (
    id                BIGSERIAL PRIMARY KEY,
    checked_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    asset             TEXT NOT NULL,
    -- What the ledger says the platform holds on the network (the custody mirror).
    ledger_minor      BIGINT NOT NULL,
    -- What the network says is there: the platform wallet plus every player's account.
    chain_minor       BIGINT NOT NULL,
    hot_minor         BIGINT NOT NULL,
    players_minor     BIGINT NOT NULL,
    -- Money on the network the ledger knows is not (yet) credited: deposits set aside for review, deposits not yet credited.
    explained_minor   BIGINT NOT NULL,
    -- chain - ledger - explained. Negative: the platform holds less than it owes. Positive: more than it records.
    difference_minor  BIGINT NOT NULL,
    -- ok            the difference is zero
    -- inconclusive  the difference is not zero but money is in motion (a withdrawal is being sent, a deposit being credited)
    -- mismatch      not zero and nothing is in motion
    verdict           TEXT NOT NULL CHECK (verdict IN ('ok', 'inconclusive', 'mismatch')),
    accounts_read     INTEGER NOT NULL CHECK (accounts_read >= 0),
    CHECK ((verdict = 'ok') = (difference_minor = 0))
);
CREATE INDEX money_checks_recent ON money_checks (asset, id DESC);
SELECT eschess_make_append_only('money_checks');

-- While a halt is active, nothing new leaves: withdrawals are not sent, staked games are not started or settled. Deposits
-- keep being credited. Only a person clears it, saying why.
CREATE TABLE money_halts (
    id            BIGSERIAL PRIMARY KEY,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    reason        TEXT NOT NULL CHECK (length(btrim(reason)) > 0),
    check_id      BIGINT REFERENCES money_checks (id),
    cleared_at    TIMESTAMPTZ,
    cleared_by    UUID REFERENCES users (id),
    clear_note    TEXT,
    CHECK ((cleared_at IS NULL) = (cleared_by IS NULL)),
    CHECK ((cleared_at IS NULL) = (clear_note IS NULL)),
    CHECK (clear_note IS NULL OR length(btrim(clear_note)) > 0)
);
-- At most one halt is active at a time.
CREATE UNIQUE INDEX money_halts_one_active ON money_halts ((true)) WHERE cleared_at IS NULL;
