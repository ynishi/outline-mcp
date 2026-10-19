-- A node's pending revision (a proposed content change waiting for approval), as JSON;
-- NULL when there is none. The node's own columns keep the current content.
ALTER TABLE nodes ADD COLUMN proposal jsonb;
