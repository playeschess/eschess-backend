-- The books-against-chain check now also reads what the escrow contract holds (ADR 0007 §7). Stakes of games on the contract
-- path sit in the contract, not in the players' accounts, so the network's total is the platform wallet plus every player's
-- account plus the contract. Older checks had no contract, so they read zero.
ALTER TABLE money_checks ADD COLUMN contract_minor BIGINT NOT NULL DEFAULT 0;
