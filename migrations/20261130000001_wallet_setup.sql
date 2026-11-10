-- Setting up a user's own Stellar account on the network (ADR 0006): requested at their first deposit intent, done by
-- the worker. The chain is the truth about whether the account is ready; this table only records that someone asked,
-- how many tries it has taken, and the last transaction sent, so a transaction that has not landed is waited for rather
-- than rebuilt.
CREATE TABLE wallet_setup (
    user_id          UUID PRIMARY KEY REFERENCES user_wallet_keys (user_id),
    requested_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    ready_at         TIMESTAMPTZ,
    attempts         INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error       TEXT,
    last_tx_hash     TEXT CHECK (last_tx_hash ~ '^[0-9a-f]{64}$'),
    last_tx_max_time BIGINT,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((last_tx_hash IS NULL) = (last_tx_max_time IS NULL))
);
CREATE INDEX wallet_setup_pending ON wallet_setup (requested_at) WHERE ready_at IS NULL;
