CREATE TABLE session_docs (
  session_id TEXT NOT NULL,
  cwd TEXT,
  updated_at TEXT,
  title TEXT,
  content TEXT,
  content_hash TEXT,
  extra_col TEXT
);
INSERT INTO session_docs (
  session_id, cwd, updated_at, title, content, content_hash, extra_col
) VALUES (
  'synthetic-session-docs-1',
  '/tmp/synthetic-cwd',
  '2026-08-01T12:00:00Z',
  'Synthetic sqlite session',
  'synthetic sqlite searchable body unique-token-session-docs-dd44',
  'synth-hash-aaaa',
  'leftover-sqlite-col'
);
