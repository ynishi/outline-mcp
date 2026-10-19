-- A retired book: kept readable, but left out of listings and context
-- injection unless asked for. Existing books are not archived.
ALTER TABLE books ADD COLUMN archived boolean NOT NULL DEFAULT false;
