-- Every user's own Stellar key (ADR 0006). The address is public; the secret key is stored only as ciphertext, sealed by
-- the key vault with the user's id and address bound in, so a row copied to another user does not open.
--
-- Rows are never changed or removed: a key is how a user's money is reached, so replacing or deleting one would strand
-- or expose funds. Moving keys to a new master key is done by adding a new version, in a later migration.
CREATE TABLE user_wallet_keys (
    user_id     UUID PRIMARY KEY REFERENCES users (id),
    address     TEXT NOT NULL UNIQUE CHECK (address ~ '^G[A-Z2-7]{55}$'),
    key_version SMALLINT NOT NULL CHECK (key_version > 0),
    nonce       BYTEA NOT NULL CHECK (length(nonce) = 12),
    -- 32 bytes of key and a 16-byte authentication tag.
    ciphertext  BYTEA NOT NULL CHECK (length(ciphertext) = 48),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
SELECT eschess_make_append_only('user_wallet_keys');
