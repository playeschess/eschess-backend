-- Stellar deposits.
--
-- All customer funds sit in one platform wallet on Stellar. A player is identified in an incoming payment by their
-- deposit id: the id of the muxed (M...) address they were given, or the memo id they were told to use.

-- Every player gets one deposit id, handed out from a sequence (so ids are unique, and start well above zero).
CREATE SEQUENCE deposit_id_seq START 100000;
CREATE TABLE deposit_ids (
    user_id    UUID PRIMARY KEY REFERENCES users (id),
    deposit_id BIGINT NOT NULL UNIQUE DEFAULT nextval('deposit_id_seq') CHECK (deposit_id > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Every payment seen arriving at the wallet, exactly once (the primary key is Horizon's operation id).
--   pending       matched to a player; the ledger posting is about to be (or is being) made
--   credited      the player's wallet has been credited
--   needs_review  money arrived that cannot be credited automatically (no deposit id, an unknown one, an asset we do not
--                 accept, or a record we could not read). It stays here for a person to look at; it is never dropped
--                 and never credited to a guess.
CREATE TABLE stellar_deposits (
    op_id          TEXT PRIMARY KEY,
    tx_hash        TEXT,
    asset          TEXT,
    amount_minor   BIGINT CHECK (amount_minor > 0),
    from_account   TEXT,
    to_account     TEXT,
    mux_id         BIGINT,
    memo_type      TEXT,
    memo           TEXT,
    user_id        UUID REFERENCES users (id),
    status         TEXT NOT NULL CHECK (status IN ('pending', 'credited', 'needs_review')),
    review_reason  TEXT,
    chain_time     TEXT,
    observed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    credited_at    TIMESTAMPTZ,
    CHECK (status <> 'credited' OR (user_id IS NOT NULL AND credited_at IS NOT NULL)),
    CHECK (status <> 'pending' OR user_id IS NOT NULL),
    CHECK (status <> 'needs_review' OR review_reason IS NOT NULL),
    -- Only a record we could not read may lack its details.
    CHECK (status = 'needs_review' OR (asset IS NOT NULL AND amount_minor IS NOT NULL))
);
CREATE INDEX stellar_deposits_user ON stellar_deposits (user_id, observed_at DESC);
CREATE INDEX stellar_deposits_review ON stellar_deposits (observed_at) WHERE status = 'needs_review';

-- Where the watcher has read up to, so a restart carries on instead of starting over.
CREATE TABLE stellar_cursors (
    name         TEXT PRIMARY KEY,
    paging_token TEXT NOT NULL,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Platform accounts ("custody_..." and "platform_..." kinds, which have no owner) are unique per kind and asset, so "the
-- custody account for USDC" is exactly one account. Other ownerless accounts are not restricted.
CREATE UNIQUE INDEX ledger_one_platform_account_per_kind_and_asset ON ledger_accounts (kind, asset)
    WHERE owner_id IS NULL AND (kind LIKE 'custody\_%' OR kind LIKE 'platform\_%');
