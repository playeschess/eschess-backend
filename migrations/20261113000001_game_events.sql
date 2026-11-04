-- Live play: every change to a game bumps `event_seq`, so a connected player can tell when they have missed an update
-- (a gap in the sequence) and ask for a fresh snapshot. The sequence only ever goes up.
ALTER TABLE games ADD COLUMN event_seq INTEGER NOT NULL DEFAULT 0 CHECK (event_seq >= 0);
