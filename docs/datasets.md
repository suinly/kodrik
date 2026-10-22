# Structured datasets

The `datasets` tool stores typed records for diaries and trackers without loading
the entire dataset into the conversation. For example, ask Kodrik to keep a
nutrition diary and report totals for a day or month. The foreground agent can
create a dataset, write records, query pages and calculate summaries.

CLI, TUI, the configured owner's Telegram chat and web chat share the owner's
datasets. Each other Telegram user/group has its own datasets. Proactive
research does not receive this tool. There is no extra configuration or database
server; data lives in `datasets/state.sqlite3` within the current space.

## Create a dataset

Tool calls use JSON. Unused top-level parameters may be omitted or set to null.
Dataset and field names start with a lowercase ASCII letter and contain only
lowercase letters, digits and underscores, at most 64 characters. Titles contain
1–200 characters. A schema contains 1–32 fields and is immutable after creation.
Creating the same definition again is safe; changing an existing definition is
an error.

```json
{
  "action": "create",
  "dataset": "nutrition",
  "title": "Nutrition diary",
  "fields": [
    { "name": "date", "type": "date", "required": true },
    { "name": "meal", "type": "string", "required": true },
    { "name": "kcal", "type": "number", "required": true },
    { "name": "protein", "type": "number", "required": false },
    { "name": "fat", "type": "number", "required": false },
    { "name": "carbs", "type": "number", "required": false }
  ],
  "date_field": "date"
}
```

Field types are `string`, `integer` (signed 64-bit), `number`, `boolean` and
`date`. Dates must be valid calendar dates written as `YYYY-MM-DD`; the optional
designated `date_field` must be a required date field. Optional values are
omitted, not null. Values are flat scalars; unknown fields, nested objects,
arrays, invalid types and duplicate fields are rejected. Strings have a 2048-byte
UTF-8 limit; a complete stored record has an 8 KiB JSON limit. Floating-point
numbers suit nutrition measurements; they are not exact decimal money values.

## Write and correct records

The tool accepts values as an array of `{field, value}` cells so its model-facing
schema stays compatible with strict function calling. Stored/query results use
ordinary JSON objects keyed by field name.

```json
{
  "action": "upsert",
  "dataset": "nutrition",
  "request_id": "d33ac304-5ff6-4141-9ab6-83513960f3a2",
  "records": [
    {
      "id": "2026-10-20-breakfast",
      "expected_revision": null,
      "values": [
        { "field": "date", "value": "2026-10-20" },
        { "field": "meal", "value": "Breakfast" },
        { "field": "kcal", "value": 350 },
        { "field": "protein", "value": 20 }
      ]
    }
  ]
}
```

Record IDs contain 1–128 ASCII letters, digits, underscores or hyphens. Null
`expected_revision` means insert only. A successful response includes committed
record revisions and the dataset revision. To correct a record, query its current
values/revision, then send the same ID, that `expected_revision`, all required
values and any optional values to preserve. Updates replace the whole record;
omitted optional fields are removed. A stale revision produces a conflict,
without overwriting a newer edit.

An upsert has 1–100 records, with no repeated IDs, and a 256 KiB request limit.
All records and the write receipt commit in one transaction. Any invalid value or
conflicting revision cancels the whole batch.

Use a new UUID `request_id` for each new write operation. If the outcome is
uncertain, repeat the original ID and payload: the tool returns the original
receipt without writing again. Reusing an ID for different data is an error.
Receipts persist even after chat history is deleted. The tool retains the
conservative automatic recovery policy used by other mixed read/write tools;
an interrupted call is not automatically re-executed.

### Import a prepared JSON batch

For existing JSON data, use a script to prepare a file containing a JSON array
of records in the same format as `records` above. Pass its path as `records_file`
and omit `records` or set it to null:

```json
{
  "action": "upsert",
  "dataset": "nutrition",
  "request_id": "d33ac304-5ff6-4141-9ab6-83513960f3a2",
  "records": null,
  "records_file": "/tmp/nutrition-batch-001.json"
}
```

The tool reads the file directly, so the model does not have to reproduce its
contents in tool arguments. The file must be UTF-8 JSON, at most 256 KiB, with
1–100 records; the expanded request must also fit the existing 256 KiB limit.
The same schema validation, revision checks, atomic transaction and write receipt
apply. Invalid batches write nothing. Use a separate UUID for each batch. Keep
file contents unchanged when retrying an uncertain write with the same UUID.
The source file is never modified.

Owner conversations read from the process filesystem, with relative paths
resolved against the process working directory. Isolated chats read only their
own persistent virtual filesystem; relative paths start at `/workspace`. A
missing virtual file never falls back to the host filesystem.

This imports prepared records, not arbitrary source formats. Inspect the target
schemas, transform the source programmatically, and compare imported counts and
totals with the source. Nested objects and arrays still require an explicit
mapping to the supported scalar fields or separate datasets.

## Discover and query

```json
{ "action": "list", "limit": 50 }
```

```json
{ "action": "describe", "dataset": "nutrition" }
```

`list` returns names/titles; `describe` returns one definition and its current
revision. `list` and `query` default to 50 rows, with a 1–200 limit. Pass a
non-null `next_cursor` back as `cursor` to continue. Query cursors are bound to
the dataset, filters and revision; if data changes, restart pagination rather
than combining pages from different versions.

```json
{
  "action": "query",
  "dataset": "nutrition",
  "date_from": "2026-10-20",
  "date_to": "2026-10-21",
  "filters": [{ "field": "meal", "value": "Breakfast" }],
  "limit": 50
}
```

The lower date boundary is inclusive; the upper boundary is exclusive. Date
ranges use the designated date field and reject datasets without one. Up to
eight typed equality filters are combined with AND. Results follow insertion
order; updating a record preserves its position. A response is at most 64 KiB,
so a page may contain fewer rows than requested. Each record includes its ID,
revision and values. Repeat the same filters on continuation calls.

## Summaries

```json
{
  "action": "summarize",
  "dataset": "nutrition",
  "date_from": "2026-10-01",
  "date_to": "2026-11-01",
  "metrics": ["kcal", "protein", "fat", "carbs"],
  "group_by": "day"
}
```

SQLite calculates matching-record count plus sum, average and contributing-value
count for up to eight numeric fields. Missing optional numbers do not contribute
to their averages. For an empty selection, an ungrouped summary returns count 0,
sum 0 and average null; a grouped summary has no groups. Omit `group_by` for
overall totals, or use `day`/`month` with a designated date field. More than 366
groups is an error: narrow the date range or group by month. Nonfinite totals
are errors, never silent nulls or partial totals.

The tool returns bounded JSON, not every historical entry. Saved query/summary
results in chat retain their original values. This is not database time travel:
there is no record edit history or whole-dataset snapshot API.

## Storage and limits

The store uses private files, WAL and transactional writes. Back up the entire
Kodrik data directory with writers stopped, or use SQLite's online backup API
for a live database-only backup; see [Storage and backups](storage.md).
Configuring an owner migrates legacy root datasets using consistent SQLite
snapshots. Active stores block migration. Identical datasets allow a retry;
different source/destination contents require manual reconciliation and are
never silently merged.

If the main database is missing while its WAL/SHM sidecars remain, startup and
migration stop without replacing them. Preserve those files and restore the
store before retrying; initializing an empty main file beside an old WAL can
silently select stale contents.

This version has no SQL execution, deletion, schema editing, automatic import of
page JSON or automatic retention. Records and receipts grow until managed
outside the tool. The default prompt and tool description ask the agent to use
dedicated tools for ordinary data/content updates. Native owner shell/patch
access remains available for development work; additional filesystem restrictions
are not required for this workflow.
Use [UI documents](ui-documents.md) to show dataset tables, totals and charts in
chat or on a published page.
