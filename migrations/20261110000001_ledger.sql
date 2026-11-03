-- The double-entry ledger: the only place a balance exists.
--
-- Four rules, all enforced here and not only in application code, so no code path can create or lose money:
--   1. A transaction's entries balance (debits = credits) for every asset, checked at COMMIT.
--   2. An account cannot be overdrawn unless it is explicitly allowed to be.
--   3. An entry's asset must be its account's asset.
--   4. Accounts, transactions and entries can be added but never changed or removed.

-- An account holds one asset. `normal_side` says which side increases it: a customer's wallet is a liability
-- (credit-normal: a credit raises the balance); the platform's treasury is an asset (debit-normal).
CREATE TABLE ledger_accounts (
    id             UUID PRIMARY KEY,
    kind           TEXT NOT NULL CHECK (length(btrim(kind)) > 0),
    asset          TEXT NOT NULL CHECK (length(asset) > 0),         -- 'XLM' or 'CODE:ISSUER'
    normal_side    TEXT NOT NULL CHECK (normal_side IN ('debit', 'credit')),
    allow_negative BOOLEAN NOT NULL DEFAULT FALSE,
    owner_id       UUID,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (id, asset)
);
CREATE INDEX ledger_accounts_owner ON ledger_accounts (owner_id) WHERE owner_id IS NOT NULL;

-- One row per posting. The idempotency key is unique, so the same request can only ever be recorded once.
CREATE TABLE ledger_transactions (
    id              UUID PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE CHECK (length(idempotency_key) BETWEEN 8 AND 128),
    fingerprint     TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (length(btrim(kind)) > 0),
    reference_type  TEXT,
    reference_id    TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((reference_type IS NULL) = (reference_id IS NULL))
);

CREATE TABLE ledger_entries (
    id             BIGSERIAL PRIMARY KEY,
    transaction_id UUID NOT NULL REFERENCES ledger_transactions (id),
    account_id     UUID NOT NULL,
    asset          TEXT NOT NULL,
    side           TEXT NOT NULL CHECK (side IN ('debit', 'credit')),
    amount_minor   BIGINT NOT NULL CHECK (amount_minor > 0),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Rule 3: the entry's asset must be the account's asset.
    FOREIGN KEY (account_id, asset) REFERENCES ledger_accounts (id, asset)
);
CREATE INDEX ledger_entries_account ON ledger_entries (account_id, id);
CREATE INDEX ledger_entries_transaction ON ledger_entries (transaction_id);

-- A running balance per account, kept by the trigger below in the same transaction as the entry, never by
-- application code. Its row lock makes concurrent postings to one account take turns, and its CHECK is
-- rule 2: the second of two racing withdrawals sees the first one's effect and fails.
CREATE TABLE ledger_balances (
    account_id     UUID PRIMARY KEY REFERENCES ledger_accounts (id),
    balance_minor  BIGINT NOT NULL,
    allow_negative BOOLEAN NOT NULL,
    CONSTRAINT ledger_balances_not_overdrawn CHECK (allow_negative OR balance_minor >= 0)
);

CREATE FUNCTION ledger_apply_entry_to_balance() RETURNS trigger AS $$
DECLARE
    account RECORD;
    delta   BIGINT;
BEGIN
    SELECT normal_side, allow_negative INTO account FROM ledger_accounts WHERE id = NEW.account_id;
    delta := CASE WHEN NEW.side = account.normal_side THEN NEW.amount_minor ELSE -NEW.amount_minor END;
    -- Create the row at zero, then change it. Doing both in one "insert or update" statement would test the
    -- overdraft rule against the proposed new row (say, -30) instead of the real resulting balance.
    INSERT INTO ledger_balances (account_id, balance_minor, allow_negative)
    VALUES (NEW.account_id, 0, account.allow_negative)
    ON CONFLICT (account_id) DO NOTHING;
    UPDATE ledger_balances SET balance_minor = balance_minor + delta WHERE account_id = NEW.account_id;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ledger_entries_balance AFTER INSERT ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_apply_entry_to_balance();

-- Rule 1. A deferred constraint trigger runs when the transaction commits, after every entry is in, and
-- aborts the commit if any asset is out of balance.
CREATE FUNCTION ledger_require_balanced() RETURNS trigger AS $$
DECLARE
    off RECORD;
BEGIN
    SELECT asset, sum(CASE WHEN side = 'debit' THEN amount_minor::numeric ELSE -amount_minor::numeric END) AS difference
      INTO off
      FROM ledger_entries
     WHERE transaction_id = NEW.transaction_id
     GROUP BY asset
    HAVING sum(CASE WHEN side = 'debit' THEN amount_minor::numeric ELSE -amount_minor::numeric END) <> 0
     LIMIT 1;
    IF FOUND THEN
        RAISE EXCEPTION 'ledger transaction % is not balanced in %: debits minus credits is %', NEW.transaction_id, off.asset, off.difference
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER ledger_entries_balanced AFTER INSERT ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION ledger_require_balanced();

-- A transaction with no entries would slip past the check above, so it is refused too.
CREATE FUNCTION ledger_require_entries() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM ledger_entries WHERE transaction_id = NEW.id) THEN
        RAISE EXCEPTION 'ledger transaction % has no entries', NEW.id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER ledger_transactions_have_entries AFTER INSERT ON ledger_transactions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION ledger_require_entries();

-- Rule 4.
SELECT eschess_make_append_only('ledger_accounts');
SELECT eschess_make_append_only('ledger_transactions');
SELECT eschess_make_append_only('ledger_entries');
