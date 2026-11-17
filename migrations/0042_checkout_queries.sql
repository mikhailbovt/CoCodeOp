ALTER TABLE checkout_file_reads ALTER COLUMN path DROP NOT NULL;
ALTER TABLE checkout_file_reads ADD COLUMN query_definition jsonb;
ALTER TABLE checkout_file_reads ADD CHECK (num_nonnulls(path,query_definition)=1);
ALTER TABLE checkout_file_reads ADD CHECK (query_definition IS NULL OR query_definition->>'kind' IN ('search','status'));
