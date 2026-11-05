-- A game is rated (counts for Glicko-2 ratings) or casual. The creator chooses when opening it; the choice cannot change
-- afterwards, so a joiner always knows what they are agreeing to. Games that already exist were all rated.
ALTER TABLE games ADD COLUMN rated BOOLEAN NOT NULL DEFAULT TRUE;
