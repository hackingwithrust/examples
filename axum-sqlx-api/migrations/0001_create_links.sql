CREATE TABLE links (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    code       TEXT NOT NULL,
    url        TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
