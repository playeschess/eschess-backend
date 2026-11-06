-- Stellar withdrawals (ADR 0001).
--
-- A withdrawal is one intent (`withdrawals`); each signed transaction built for it is an attempt (`withdrawal_attempts`).
-- Money moves only through the ledger, in the same database transaction as the status change that causes it:
--   hold    on request:      user_wallet -> platform_withdrawals_pending
--   settle  on confirmation: platform_withdrawals_pending -> custody_stellar
--   refund  on failure, cancellation or rejection: platform_withdrawals_pending -> user_wallet
--
-- The rules that keep money safe are enforced here, not only in application code:
--   - a withdrawal moves only forward through its states, and its amount, asset and destination never change;
--   - a withdrawal is held once, and is either settled or refunded, never both, never twice;
--   - at most one attempt per withdrawal is unresolved at a time, so a new transaction is never sent while an older
--     one could still land;
--   - nobody approves their own withdrawal.

-- People allowed to do things players cannot. Granted by hand (SQL) for now.
CREATE TABLE user_roles (
    user_id    UUID NOT NULL REFERENCES users (id),
    role       TEXT NOT NULL CHECK (role IN ('withdrawal_approver')),
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, role)
);

-- awaiting_approval  above the auto-approve threshold; waiting for a person
-- approved           may be sent; nothing has been signed yet
-- submitting         at least one attempt exists: a signed transaction may be in flight, so it can no longer be cancelled
-- confirmed          the payment landed; settled in the ledger                       (final)
-- failed             every attempt is proved not to have paid; refunded               (final)
-- cancelled          withdrawn by the player before anything was signed; refunded    (final)
-- rejected           turned down by an approver; refunded                            (final)
CREATE TABLE withdrawals (
    id                UUID PRIMARY KEY,
    user_id           UUID NOT NULL REFERENCES users (id),
    -- Chosen by the app, so a retried request finds the first one instead of making a second withdrawal.
    client_request_id UUID NOT NULL,
    asset             TEXT NOT NULL CHECK (length(asset) > 0),
    amount_minor      BIGINT NOT NULL CHECK (amount_minor > 0),
    destination       TEXT NOT NULL CHECK (destination ~ '^[GM][A-Z2-7]{55}([A-Z2-7]{13})?$'),
    memo_type         TEXT NOT NULL CHECK (memo_type IN ('none', 'id', 'text', 'hash', 'return')),
    memo              TEXT,
    status            TEXT NOT NULL CHECK (status IN ('awaiting_approval', 'approved', 'submitting', 'confirmed', 'failed', 'cancelled', 'rejected')),
    -- Who approved or rejected it; NULL when it was approved automatically.
    decided_by        UUID REFERENCES users (id),
    decided_at        TIMESTAMPTZ,
    failure_reason    TEXT,
    requested_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at       TIMESTAMPTZ,
    UNIQUE (user_id, client_request_id),
    CHECK ((memo_type = 'none') = (memo IS NULL)),
    CHECK (decided_by IS NULL OR decided_by <> user_id),
    CHECK (status <> 'rejected' OR decided_by IS NOT NULL),
    CHECK (status NOT IN ('failed', 'rejected') OR failure_reason IS NOT NULL),
    CHECK ((status IN ('confirmed', 'failed', 'cancelled', 'rejected')) = (finished_at IS NOT NULL))
);
CREATE INDEX withdrawals_user ON withdrawals (user_id, requested_at DESC);
CREATE INDEX withdrawals_open ON withdrawals (status, requested_at)
    WHERE status IN ('awaiting_approval', 'approved', 'submitting');

CREATE FUNCTION withdrawals_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.id, NEW.user_id, NEW.client_request_id, NEW.asset, NEW.amount_minor, NEW.destination, NEW.memo_type,
        NEW.memo, NEW.requested_at)
       IS DISTINCT FROM
       (OLD.id, OLD.user_id, OLD.client_request_id, OLD.asset, OLD.amount_minor, OLD.destination, OLD.memo_type,
        OLD.memo, OLD.requested_at) THEN
        RAISE EXCEPTION 'withdrawal %: what, how much and to whom cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.status IS DISTINCT FROM OLD.status AND NOT (
           (OLD.status = 'awaiting_approval' AND NEW.status IN ('approved', 'rejected', 'cancelled'))
        OR (OLD.status = 'approved'          AND NEW.status IN ('submitting', 'cancelled'))
        OR (OLD.status = 'submitting'        AND NEW.status IN ('confirmed', 'failed'))
    ) THEN
        RAISE EXCEPTION 'withdrawal %: % cannot become %', OLD.id, OLD.status, NEW.status USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.status = OLD.status AND OLD.status IN ('confirmed', 'failed', 'cancelled', 'rejected')
       AND ROW(NEW.*) IS DISTINCT FROM ROW(OLD.*) THEN
        RAISE EXCEPTION 'withdrawal % is finished and cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER withdrawals_guard BEFORE UPDATE ON withdrawals
    FOR EACH ROW EXECUTE FUNCTION withdrawals_guard();
CREATE TRIGGER withdrawals_no_delete BEFORE DELETE OR TRUNCATE ON withdrawals
    FOR EACH STATEMENT EXECUTE FUNCTION eschess_refuse_change();

-- pending  signed and recorded; it may have been submitted, and it may yet land
-- success  it landed and paid
-- failed   it landed and did nothing but charge its fee (a result code says why)
-- expired  proved never to land: the network closed a ledger after max_time and the hash is not on chain
CREATE TABLE withdrawal_attempts (
    id                   UUID PRIMARY KEY,
    withdrawal_id        UUID NOT NULL REFERENCES withdrawals (id),
    attempt_no           INTEGER NOT NULL CHECK (attempt_no > 0),
    tx_hash              TEXT NOT NULL UNIQUE CHECK (tx_hash ~ '^[0-9a-f]{64}$'),
    source_account       TEXT NOT NULL CHECK (source_account ~ '^G[A-Z2-7]{55}$'),
    sequence             BIGINT NOT NULL CHECK (sequence > 0),
    fee_stroops          BIGINT NOT NULL CHECK (fee_stroops > 0),
    max_time             BIGINT NOT NULL CHECK (max_time > 0),   -- unix seconds; after this it can never land
    envelope_xdr         TEXT NOT NULL CHECK (length(envelope_xdr) > 0),
    status               TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'success', 'failed', 'expired')),
    result_code          TEXT,
    fee_charged_stroops  BIGINT CHECK (fee_charged_stroops >= 0),
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at          TIMESTAMPTZ,
    UNIQUE (withdrawal_id, attempt_no),
    CHECK ((status = 'pending') = (resolved_at IS NULL)),
    CHECK (status <> 'failed' OR result_code IS NOT NULL)
);
-- The rule that prevents the double payout: one unresolved attempt per withdrawal.
CREATE UNIQUE INDEX withdrawal_attempts_one_open ON withdrawal_attempts (withdrawal_id) WHERE status = 'pending';
-- A transaction that landed used up its sequence number; one that might still land could. Two such transactions on
-- one sequence number is a bug (only one can ever apply), so it is refused. An expired one used up nothing.
CREATE UNIQUE INDEX withdrawal_attempts_sequence ON withdrawal_attempts (source_account, sequence)
    WHERE status <> 'expired';
CREATE INDEX withdrawal_attempts_pending ON withdrawal_attempts (created_at) WHERE status = 'pending';

CREATE FUNCTION withdrawal_attempts_guard() RETURNS trigger AS $$
BEGIN
    IF (NEW.id, NEW.withdrawal_id, NEW.attempt_no, NEW.tx_hash, NEW.source_account, NEW.sequence, NEW.fee_stroops,
        NEW.max_time, NEW.envelope_xdr, NEW.created_at)
       IS DISTINCT FROM
       (OLD.id, OLD.withdrawal_id, OLD.attempt_no, OLD.tx_hash, OLD.source_account, OLD.sequence, OLD.fee_stroops,
        OLD.max_time, OLD.envelope_xdr, OLD.created_at) THEN
        RAISE EXCEPTION 'withdrawal attempt %: a signed transaction cannot change', OLD.id USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.status <> 'pending' AND ROW(NEW.*) IS DISTINCT FROM ROW(OLD.*) THEN
        RAISE EXCEPTION 'withdrawal attempt % is resolved (%) and cannot change', OLD.id, OLD.status USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER withdrawal_attempts_guard BEFORE UPDATE ON withdrawal_attempts
    FOR EACH ROW EXECUTE FUNCTION withdrawal_attempts_guard();
CREATE TRIGGER withdrawal_attempts_no_delete BEFORE DELETE OR TRUNCATE ON withdrawal_attempts
    FOR EACH STATEMENT EXECUTE FUNCTION eschess_refuse_change();

-- In the ledger: one hold per withdrawal, and one outcome (settle or refund, not both).
CREATE UNIQUE INDEX ledger_one_withdrawal_hold ON ledger_transactions (reference_id)
    WHERE reference_type = 'withdrawal' AND kind = 'withdrawal_hold';
CREATE UNIQUE INDEX ledger_one_withdrawal_outcome ON ledger_transactions (reference_id)
    WHERE reference_type = 'withdrawal' AND kind IN ('withdrawal_settle', 'withdrawal_refund');
