-- Settling through the contract (ADR 0007 §3). The outcome a settle transaction told the contract is recorded with it, so
-- that if the worker dies after the network settled the match, the next round can post the same pot in the ledger from
-- what the network was told, never from a fresh decision that might differ.
ALTER TABLE contract_txs ADD COLUMN detail TEXT;
