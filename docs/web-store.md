# Web app: designs on a host's disk (`?store=<url>`)

By default the web app keeps designs in the browser's private storage (OPFS): they exist in
that one browser on that one device. Opened with `?store=<url>`, it keeps them in an HTTP
**design store** instead, for example a folder on a home server that every device reaches. The
store is the source of truth; the browser keeps a working copy for speed, offline starts and the
recovery autosave.

The store's URL must be the page's own origin or loopback (`127.0.0.1`, `localhost`, `[::1]`),
over http or https; anything else is ignored, so a link can't send someone's designs elsewhere.
A store on another loopback port needs CORS headers that allow `If-Match`, `If-None-Match` and
`Content-Type` and expose `ETag`.

## What the page does

- **Start page and Open:** the recent designs and the **Designs** panel list the store's
  designs ("on disk"). Designs that were only in this browser are listed under **Only in this
  browser**, with **Copy browser designs to disk** (also offered once when the page starts).
  A design opened from the computer is copied to the store.
- **Save / Save As** write to the store. Save As never replaces another design: a taken name
  gets `name (2)`, `name (3)`… The status line says "Saving … to disk…", then "… saved to disk".
- **Conflicts:** each open design remembers the version it was opened from, and a save says
  which version it replaces. If the file changed meanwhile (another device, another program,
  git), nothing is replaced: the save goes beside it as `<name> (conflict <date> <time>).<ext>`,
  the status line says so, and the open design saves to that file from then on.
- **Offline:** when the store can't be reached, the save stays in the browser and the page says
  **NOT on disk yet** (status line, and a badge with **Push now** until it is sent). It is sent
  again with backoff, when the store answers again, and after a reload (the waiting bytes are
  kept in OPFS). Leaving the page while something is not on disk asks first.
- **Changes from elsewhere** appear within moments: the page holds a listing request open until
  the store's listing changes. A design that changed on disk while it is open here is not
  reloaded under the person; the status line says it changed.

## Protocol

All paths are under the store URL. Names are bare file names: no `/` or `\`, no leading dot,
no control characters, ending in `.solidcraft` or `.json`, at most 200 bytes. An **etag** names
one version of a file's bytes (a content hash is recommended, so restoring the same bytes is no
change).

| Request | Answer |
| --- | --- |
| `GET designs` | `{"version": "<listing version>", "designs": [{"name", "size", "modified" (ms since 1970), "etag"}]}` |
| `GET designs?wait=<version>` | The same, held (up to ~25 s) until the listing is no longer `<version>`. A store may answer at once; the page then asks every few seconds. |
| `GET designs/<name>` | The bytes, with `ETag: "<etag>"`; 404 when there is no such design. |
| `PUT designs/<name>` with `If-Match: "<etag>"` | Replace the version `<etag>`: 200 `{"name", "etag", "size", "modified"}`. **412** `{"error", "etag": <current or null>}` when the file is no longer that version: nothing is written. |
| `PUT designs/<name>` with `If-None-Match: *` | Create: 201 as above; 412 when it exists. |

A `PUT` without either header may be refused with 428; a bad name with 400; a design that is
too large with 413. Writes should be atomic (a reader sees the old or the new bytes, never
part). The page treats 5xx, 408, 429 and network errors as "not reachable" (it retries) and
other 4xx as a refusal (the design waits in the browser until **Push now**).

There is no delete or rename: designs on the store are managed where the store is.

## Implementation

`apps/solidcraft-web/src/store.rs`: stored designs appear to the engine under `/store/` (beside
`/opfs/` and `/downloads/`), so opening, the start page and the save commands work unchanged; the
store module sends what the save commands write, applies what changed on the store when no open
design uses it, and keeps its copy in an OPFS folder `store-<hash of the URL>/`.
