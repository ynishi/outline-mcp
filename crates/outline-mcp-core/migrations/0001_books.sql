-- outline-mcp: books, their nodes, and the change events that produced them.
--
-- One write is one transaction over all three tables: the book row's version
-- is checked and bumped, only the node rows that changed are written, and the
-- write's events are inserted. A write either stores its events or nothing.

CREATE TABLE books (
    slug        text        PRIMARY KEY,
    book_id     uuid        NOT NULL,
    title       text        NOT NULL,
    max_depth   smallint    NOT NULL,
    version     bigint      NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- `position` is the node's index among its siblings (root nodes: parent NULL).
-- `updated_at` is the node's own Unix-millis stamp, NULL for nodes imported
-- from files that predate it. Node ids are unique per book only: a book copied
-- under a second slug keeps its ids.
CREATE TABLE nodes (
    book        text        NOT NULL REFERENCES books (slug) ON DELETE CASCADE,
    id          uuid        NOT NULL,
    parent      uuid,
    position    integer     NOT NULL,
    title       text        NOT NULL,
    body        text,
    node_type   text        NOT NULL,
    placeholder text,
    properties  jsonb       NOT NULL DEFAULT '{}'::jsonb,
    status      text        NOT NULL,
    updated_at  bigint,
    PRIMARY KEY (book, id)
);

CREATE INDEX nodes_book_parent_position ON nodes (book, parent, position);

-- Append-only. `before` / `after` hold the whole node as it was / became, so
-- history is read row by row and never depends on replaying earlier events.
CREATE TABLE events (
    id          bigserial   PRIMARY KEY,
    book        text        NOT NULL REFERENCES books (slug) ON DELETE CASCADE,
    node_id     uuid,
    action      text        NOT NULL,
    before      jsonb,
    after       jsonb,
    at          bigint      NOT NULL
);

CREATE INDEX events_book_node ON events (book, node_id, id);
