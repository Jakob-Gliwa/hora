# On-demand status summary and compression

The status summary behind the page, `/api/summary` and `/metrics` used to be
rebuilt every 5 seconds whether or not anybody was looking. On a small
installation that rebuild was most of Hora's CPU time, and it grows with the
stored history.

It is now built on demand:

- A request served from a snapshot younger than 5 s costs nothing extra.
- An older snapshot is served at once and wakes a rebuild in the background
  (stale-while-revalidate), so the next request is fresh.
- A snapshot older than 30 s is not served: the request waits for the rebuild
  it triggered (at most 5 s, typically well under a second), so a viewer
  returning after a quiet spell never sees an old status.
- Without requests the summary is rebuilt every 5 minutes, which keeps its
  incremental caches warm.
- Config reloads and operator writes (announcements, events, silences) still
  rebuild immediately, and a build never starts sooner than its own duration
  after the previous one ended.

Checks, alerts, reminders and heartbeats never depend on the summary; only
what the page and the API show does.

Responses are gzip-compressed when the client sends `Accept-Encoding: gzip`.
`/api/summary` carries the displayed history of every monitor and shrinks to a
fraction of its size; tiny responses and images are left alone.
