#!/usr/bin/env python3 -I
"""Converts a copy of the prototype's data.db into the cockpit's schema, for validation only.

    python3 -I scripts/proto-to-cockpit.py PROTO_DATA_DB OUT_DB

The source is copied to a temp dir and read from the copy (never written). OUT_DB must not exist.
Linked meeting files from the prototype's `context/` are copied next to OUT_DB (`OUT_DB` with `.context`),
where the cockpit looks for them, and the links are rewritten to point there.
Never point OUT_DB at the real install (~/Library/Application Support/cockpit).
"""
import os
import shutil
import sqlite3
import sys
import tempfile
from datetime import datetime, timezone

# Mirrors MIGRATIONS[0] in src/db.rs (user_version 1); the cockpit migrates further when it opens the file.
SCHEMA = """
CREATE TABLE tickets(key TEXT PRIMARY KEY, repo TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'todo', priority INTEGER NOT NULL DEFAULT 3, project TEXT, cluster TEXT, blocked_by TEXT,
  session TEXT, rank REAL NOT NULL DEFAULT 0, created TEXT NOT NULL, updated TEXT NOT NULL);
CREATE INDEX tickets_project ON tickets(project);
CREATE TABLE sessions(id TEXT PRIMARY KEY, repo TEXT, cwd TEXT, started TEXT, last_seen TEXT, ended TEXT,
  head_start TEXT, head_end TEXT, tmux_pane TEXT, state TEXT, state_detail TEXT, state_at TEXT,
  transcript TEXT, last_message TEXT, last_prompt TEXT, trail TEXT);
CREATE TABLE events(id INTEGER PRIMARY KEY, ticket TEXT, session TEXT, kind TEXT NOT NULL, text TEXT NOT NULL DEFAULT '', at TEXT NOT NULL);
CREATE INDEX events_ticket ON events(ticket);
CREATE TABLE repos(name TEXT PRIMARY KEY, path TEXT NOT NULL);
CREATE TABLE projects(name TEXT PRIMARY KEY, palaver TEXT, services TEXT);
CREATE TABLE prompts(session TEXT NOT NULL, at TEXT NOT NULL);
CREATE INDEX prompts_at ON prompts(at);
CREATE TABLE links(ticket TEXT, kind TEXT, ref TEXT, title TEXT, path TEXT, why TEXT, at TEXT, PRIMARY KEY(ticket, kind, ref));
CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
PRAGMA user_version=1;
"""

NOW = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z")


def main(src, out):
    if os.path.exists(out):
        sys.exit(f"{out} exists; refusing to overwrite")
    proto_dir = os.path.dirname(os.path.abspath(src))
    work = tempfile.mkdtemp(prefix="proto-copy-")
    try:
        copy = os.path.join(work, "data.db")
        shutil.copy2(src, copy)
        s = sqlite3.connect(f"file:{copy}?mode=ro", uri=True)
        os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
        d = sqlite3.connect(out)
        d.executescript(SCHEMA)
        counts = {}

        def put(table, sql, rows):
            rows = list(rows)
            d.executemany(sql, rows)
            counts[table] = len(rows)

        # issues -> tickets (new columns are NOT NULL; the prototype allowed NULLs)
        put("tickets", "INSERT INTO tickets VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)", (
            (k, r, t, b or "", st or "todo", p if p is not None else 3, pr, cl, bb, se,
             rk if rk is not None else 0, cr or NOW, up or cr or NOW)
            for k, r, t, b, st, p, pr, bb, se, cr, up, rk, cl in s.execute(
                "SELECT key,repo,title,body,status,priority,project,blocked_by,session,created,updated,rank,cluster FROM issues"
            )
        ))
        put("sessions", "INSERT INTO sessions VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", s.execute(
            "SELECT id,repo,cwd,started,last_seen,ended,head_start,head_end,tmux_pane,state,state_detail,state_at,"
            "transcript,last_message,last_prompt,trail FROM sessions"))
        put("events", "INSERT INTO events(id,ticket,session,kind,text,at) VALUES(?,?,?,?,?,?)", (
            (i, t, se, k, tx or "", at or NOW) for i, t, se, k, tx, at in s.execute(
                "SELECT id,issue,session,kind,text,at FROM events")))
        put("repos", "INSERT INTO repos VALUES(?,?)", s.execute("SELECT name,path FROM repos WHERE path IS NOT NULL"))
        put("projects", "INSERT INTO projects VALUES(?,?,?)", s.execute("SELECT name,palaver,services FROM projects"))
        # tickets name projects the prototype never registered; the cockpit lists projects from this table
        cur = d.execute(
            "INSERT OR IGNORE INTO projects(name) SELECT DISTINCT project FROM tickets WHERE project IS NOT NULL")
        counts["projects (added from tickets)"] = cur.rowcount

        ctx_src = os.path.join(proto_dir, "context")
        ctx_out = os.path.splitext(os.path.abspath(out))[0] + ".context"

        def link_rows():
            for t, k, r, ti, path, why, at in s.execute("SELECT ticket,kind,ref,title,path,why,at FROM links"):
                if path and path.startswith(ctx_src + os.sep):
                    os.makedirs(ctx_out, exist_ok=True)
                    new = os.path.join(ctx_out, os.path.basename(path))
                    if os.path.exists(path):
                        shutil.copy2(path, new)
                    path = new
                yield t, k, r, ti, path, why, at

        put("links", "INSERT INTO links VALUES(?,?,?,?,?,?,?)", link_rows())
        put("prompts", "INSERT INTO prompts VALUES(?,?)", s.execute("SELECT session,at FROM prompts WHERE at IS NOT NULL"))
        put("settings", "INSERT INTO settings VALUES(?,?)", s.execute("SELECT key,value FROM settings WHERE value IS NOT NULL"))
        d.commit()
        d.close()
        s.close()
        for t, n in counts.items():
            print(f"{t:32}{n}")
        print(f"wrote {out}")
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(*sys.argv[1:])
