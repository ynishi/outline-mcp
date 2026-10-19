-- When someone last confirmed a node still matches reality (`node_verify`),
-- and who. Unix millis like `updated_at`; NULL = never verified.
ALTER TABLE nodes ADD COLUMN verified_at bigint;
ALTER TABLE nodes ADD COLUMN verified_by text;
