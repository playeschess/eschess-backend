-- When to look at each player's own address for deposits (ADR 0006 §3). Looking at every address every few seconds does
-- not scale, so an address is checked often while its owner is likely to be depositing (they just asked how, or money just
-- arrived) and rarely otherwise. A payment is never lost by being looked at late: the position read up to is kept per
-- address (stellar_cursors), so a late look sees everything since the last one.
CREATE TABLE wallet_watch (
    user_id          UUID PRIMARY KEY REFERENCES user_wallet_keys (user_id),
    last_checked_at  TIMESTAMPTZ,
    -- The last time the owner asked how to deposit or money arrived. Decides how often to look.
    last_activity_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    next_check_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX wallet_watch_due ON wallet_watch (next_check_at);
