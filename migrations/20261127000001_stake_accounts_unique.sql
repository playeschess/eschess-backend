-- "The stake escrow account for USDC" must be exactly one account, like the custody and platform accounts. The earlier
-- index (20261118) covers only custody_* and platform_* kinds, so stake_escrow and stake_fees were unprotected and each
-- lookup could create another. Ledger accounts cannot be deleted, so a database that already grew extras keeps them
-- (harmless: unused, balance 0, and the code now always picks the oldest); only a database without extras gets the
-- index.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM ledger_accounts WHERE owner_id IS NULL AND kind LIKE 'stake\_%'
        GROUP BY kind, asset HAVING count(*) > 1
    ) THEN
        CREATE UNIQUE INDEX ledger_one_stake_account_per_kind_and_asset ON ledger_accounts (kind, asset)
            WHERE owner_id IS NULL AND kind LIKE 'stake\_%';
    END IF;
END $$;
