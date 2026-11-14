-- Fiat in and out through a Stellar anchor (SEP-24). One row per time a player opens the anchor's screens to add money from, or
-- cash out to, their own currency.
--
-- The anchor holds the fiat and we never do. A deposit is the anchor sending USDC to the player's own address, which the
-- deposit watcher credits like any other payment. A cash-out is a withdrawal like any other (limits, approval, the fair-play
-- hold) from the player's own account to the anchor's account with the memo the anchor gave; `withdrawal_id` says which.
-- The address of the anchor's screen carries a login token and is never stored.

CREATE TABLE fiat_sessions (
    id             UUID PRIMARY KEY,
    user_id        UUID NOT NULL REFERENCES users (id),
    kind           TEXT NOT NULL CHECK (kind IN ('deposit', 'withdraw')),
    anchor_domain  TEXT NOT NULL CHECK (length(btrim(anchor_domain)) > 0),
    -- The anchor's own id for the transaction.
    anchor_tx_id   TEXT NOT NULL CHECK (length(btrim(anchor_tx_id)) > 0),
    -- in_progress  the player is in the anchor's screens, or the anchor is working
    -- sending      a cash-out's USDC is being sent to the anchor (the withdrawal is made)
    -- completed / failed / expired    final
    status         TEXT NOT NULL DEFAULT 'in_progress' CHECK (status IN ('in_progress', 'sending', 'completed', 'failed', 'expired')),
    anchor_status  TEXT,
    -- What the anchor asked a cash-out to send, in USDC minor units, once it said.
    amount_in_minor BIGINT CHECK (amount_in_minor IS NULL OR amount_in_minor > 0),
    message        TEXT,
    more_info_url  TEXT,
    -- The cash-out's withdrawal, made once.
    withdrawal_id  UUID UNIQUE REFERENCES withdrawals (id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at    TIMESTAMPTZ,
    UNIQUE (anchor_domain, anchor_tx_id),
    CHECK ((status IN ('completed', 'failed', 'expired')) = (finished_at IS NOT NULL)),
    CHECK (withdrawal_id IS NULL OR kind = 'withdraw')
);
CREATE INDEX fiat_sessions_user ON fiat_sessions (user_id, created_at DESC);
CREATE INDEX fiat_sessions_open ON fiat_sessions (updated_at) WHERE finished_at IS NULL;
