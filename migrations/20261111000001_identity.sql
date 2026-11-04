-- Players, and the sessions they sign in with.

CREATE TABLE users (
    id                 UUID PRIMARY KEY,
    email              TEXT NOT NULL CHECK (email = lower(email) AND length(email) <= 254),
    username           TEXT NOT NULL CHECK (username ~ '^[a-z0-9_]{3,20}$'),
    password_hash      TEXT NOT NULL,                       -- Argon2id; the password itself is never stored
    status             TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    failed_login_count INTEGER NOT NULL DEFAULT 0,
    locked_until       TIMESTAMPTZ,
    email_verified_at  TIMESTAMPTZ,                       -- NULL until the player proves they can read this mailbox
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email_unique ON users (email);
CREATE UNIQUE INDEX users_username_unique ON users (username);

-- One-time tokens sent by email: to confirm an address, or to reset a password. Only the hash is stored, each
-- works once, and each expires. Asking for a new one cancels the player's earlier unused ones.
CREATE TABLE email_tokens (
    id         UUID PRIMARY KEY,
    user_id    UUID NOT NULL REFERENCES users (id),
    email      TEXT NOT NULL,                       -- the address it was sent to, so it cannot confirm a different one
    purpose    TEXT NOT NULL CHECK (purpose IN ('verify_email', 'reset_password')),
    token_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);
CREATE UNIQUE INDEX email_tokens_hash ON email_tokens (token_hash);
CREATE INDEX email_tokens_user ON email_tokens (user_id, purpose);

-- Every email sent, so one address cannot be flooded by repeated requests.
CREATE TABLE email_log (
    id         BIGSERIAL PRIMARY KEY,
    email      TEXT NOT NULL,
    kind       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX email_log_recent ON email_log (email, kind, created_at DESC);

-- A session is one signed-in device. Only hashes of tokens are stored, so a copy of this table cannot be
-- used to sign in. The access token is short-lived and replaced on every refresh.
CREATE TABLE sessions (
    id                  UUID PRIMARY KEY,
    user_id             UUID NOT NULL REFERENCES users (id),
    device_label        TEXT,
    access_hash         TEXT NOT NULL,
    access_expires_at   TIMESTAMPTZ NOT NULL,
    absolute_expires_at TIMESTAMPTZ NOT NULL,       -- the session ends here no matter how often it is refreshed
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at          TIMESTAMPTZ,
    revoked_reason      TEXT
);
CREATE UNIQUE INDEX sessions_access_hash ON sessions (access_hash);
CREATE INDEX sessions_user ON sessions (user_id, created_at DESC);

-- Refresh tokens are single use: each refresh spends one and issues the next. Presenting a spent one means
-- two parties hold the same token, so the whole session is revoked.
CREATE TABLE refresh_tokens (
    id         UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions (id),
    token_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    used_at    TIMESTAMPTZ
);
CREATE UNIQUE INDEX refresh_tokens_hash ON refresh_tokens (token_hash);
CREATE INDEX refresh_tokens_session ON refresh_tokens (session_id);

-- Ledger accounts now belong to real users, and each user has at most one wallet per asset.
ALTER TABLE ledger_accounts ADD CONSTRAINT ledger_accounts_owner_fk FOREIGN KEY (owner_id) REFERENCES users (id);
CREATE UNIQUE INDEX ledger_one_wallet_per_user_and_asset ON ledger_accounts (owner_id, asset) WHERE kind = 'user_wallet';
