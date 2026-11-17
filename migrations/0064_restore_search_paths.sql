-- pg_restore creates generated columns with an empty session search path.
ALTER FUNCTION public.office_claim_ranges(jsonb) SET search_path = pg_catalog, public;
