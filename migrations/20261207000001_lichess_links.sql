-- Which Lichess account each player plays with (ADR 0005). A player proves they own the account by logging in at Lichess;
-- we keep only the account's id and username, never a Lichess token.
--
-- Both ways are one-to-one and permanent: one player has one Lichess account, and one Lichess account belongs to one
-- player. Money rides on games played under that account, so it can be neither shared between players nor swapped out
-- from under a game; a change is a decision for a person, made in the database.

-- One login attempt, found again by its `state` when the player comes back from Lichess. The verifier is the one-time
-- secret of this attempt; it is useless without the code Lichess sent to the player's own browser.
CREATE TABLE lichess_link_attempts (
    state      TEXT PRIMARY KEY CHECK (length(state) >= 32),
    user_id    UUID NOT NULL REFERENCES users (id),
    verifier   TEXT NOT NULL CHECK (length(verifier) BETWEEN 43 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);
CREATE INDEX lichess_link_attempts_user ON lichess_link_attempts (user_id, created_at DESC);

CREATE TABLE lichess_links (
    user_id     UUID PRIMARY KEY REFERENCES users (id),
    -- Lichess's lowercase account id.
    lichess_id  TEXT NOT NULL UNIQUE CHECK (lichess_id ~ '^[a-z0-9_-]{1,30}$'),
    username    TEXT NOT NULL CHECK (length(btrim(username)) > 0),
    linked_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT eschess_make_append_only('lichess_links');
