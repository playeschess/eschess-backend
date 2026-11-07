-- Sanctions (ADR 0003 §3): what a confirmed fair-play case does to the player. One per confirmed case, written in the
-- same transaction as the decision, never changed or removed.
CREATE TABLE fairplay_sanctions (
    id         UUID PRIMARY KEY,
    case_id    UUID NOT NULL UNIQUE REFERENCES fairplay_cases (id),
    player_id  UUID NOT NULL REFERENCES users (id),
    kind       TEXT NOT NULL CHECK (kind IN ('warning', 'suspension')),
    imposed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- A suspension has an end; a warning restricts nothing and has none.
    ends_at    TIMESTAMPTZ,
    CHECK ((kind = 'suspension') = (ends_at IS NOT NULL)),
    CHECK (ends_at IS NULL OR ends_at > imposed_at)
);
CREATE INDEX fairplay_sanctions_player ON fairplay_sanctions (player_id, ends_at);

-- The sanction belongs to a confirmed case about the same player.
CREATE FUNCTION fairplay_sanctions_guard() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM fairplay_cases WHERE id = NEW.case_id AND player_id = NEW.player_id AND status = 'confirmed') THEN
        RAISE EXCEPTION 'fairplay sanction: case % is not a confirmed case about player %', NEW.case_id, NEW.player_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE CONSTRAINT TRIGGER fairplay_sanctions_guard AFTER INSERT ON fairplay_sanctions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fairplay_sanctions_guard();

CREATE FUNCTION fairplay_sanctions_immutable() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'fairplay sanctions are append-only' USING ERRCODE = 'check_violation';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER fairplay_sanctions_no_change BEFORE UPDATE OR DELETE OR TRUNCATE ON fairplay_sanctions
    FOR EACH STATEMENT EXECUTE FUNCTION fairplay_sanctions_immutable();
