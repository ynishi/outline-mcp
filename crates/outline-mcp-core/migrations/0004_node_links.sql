-- Outgoing typed links of a node: a JSON array of {"rel": ..., "target": <node uuid>}.
-- Backlinks are derived when a book is read; nothing else indexes this column.
ALTER TABLE nodes ADD COLUMN links jsonb NOT NULL DEFAULT '[]';
