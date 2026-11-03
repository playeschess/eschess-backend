-- A reusable guard for tables that must never be edited or emptied: the ledger, the audit log, settlement
-- attempts. "Append-only by policy" is not append-only, so the database itself refuses.
--
--   SELECT eschess_make_append_only('ledger_entries');
--
-- After that, INSERT works and UPDATE, DELETE and TRUNCATE on the table raise an error, for every role.

CREATE FUNCTION eschess_refuse_change() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: % is not allowed', TG_TABLE_NAME, TG_OP
        USING ERRCODE = 'integrity_constraint_violation';
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION eschess_make_append_only(target regclass) RETURNS void AS $$
BEGIN
    EXECUTE format('CREATE TRIGGER eschess_no_update BEFORE UPDATE ON %s FOR EACH ROW EXECUTE FUNCTION eschess_refuse_change()', target);
    EXECUTE format('CREATE TRIGGER eschess_no_delete BEFORE DELETE ON %s FOR EACH ROW EXECUTE FUNCTION eschess_refuse_change()', target);
    EXECUTE format('CREATE TRIGGER eschess_no_truncate BEFORE TRUNCATE ON %s FOR EACH STATEMENT EXECUTE FUNCTION eschess_refuse_change()', target);
END;
$$ LANGUAGE plpgsql;
