-- outline-mcp: whole-book snapshots.
--
-- One row per snapshot, keyed by the millisecond it was taken (the value the
-- snapshot tools take as `timestamp`). `tree` is the book as it was, in the
-- same JSON shape the book serializes to. Restoring a snapshot is an ordinary
-- write through `books` / `nodes` / `events`, so this table is never read to
-- decide the current book.

CREATE TABLE snapshots (
    book        text        NOT NULL REFERENCES books (slug) ON DELETE CASCADE,
    taken_at    bigint      NOT NULL,
    label       text,
    tree        jsonb       NOT NULL,
    PRIMARY KEY (book, taken_at)
);
