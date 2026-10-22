# Native UI documents

The `show_ui` tool renders Markdown with a package-derived MDC component catalog in web
chat. Supplying a page `slug` publishes the same document at `/pages/<slug>/`.
Application code owns the Nuxt UI components and Catppuccin Blue theme; the
agent supplies content, literal properties and named data sources.

For example, a chat-only table from an existing nutrition dataset:

```json
{
  "title": "Nutrition",
  "body": "# Nutrition\n\n::data-table{source=\"meals\" title=\"Meal history\"}\n::",
  "sources": [{
    "id": "meals",
    "data_json": null,
    "query": {
      "mode": "query",
      "dataset": "nutrition",
      "date_from": "2026-10-01",
      "date_to": "2026-11-01",
      "filters": null,
      "metrics": null,
      "group_by": null,
      "limit": 50
    }
  }],
  "slug": null,
  "visibility": null
}
```

`mode` is `query` or `summarize`, using the same corresponding parameters and
limits as [datasets](datasets.md). A source can instead provide `data_json`, a
JSON string containing an array of flat scalar row objects, with `query: null`.
Exactly one data mode is required. Source IDs use 1–64 ASCII letters, digits or
underscores and must be unique in the document. Inline data permits null for an
unknown value; it does not create durable records.

### Rolling periods and computed views

Dataset queries can use `last_days` (1–366) and an IANA `timezone`, e.g.
`"last_days": 7, "timezone": "Europe/Moscow"`, with `date_from` and `date_to`
omitted or null. This includes the local current day and the preceding six
calendar days. The default timezone is UTC; use the user's timezone explicitly.
The period is resolved on every private page load/refresh. Absolute date bounds
cannot be combined with `last_days`.

`show_ui.views` accepts up to eight `{id, source, jq}` definitions. `source` names
one declared source; jq receives its complete flat row array. Record envelopes
are flattened to `{id, ...values}`, and summaries expose `group`, `count` and
numeric sums by metric name. Views cannot consume other views. For example:

```json
[
  {"id":"week","source":"days","jq":"sort_by(.date)"},
  {"id":"latest","source":"days","jq":"sort_by(.date) | last"}
]
```

Bind computed JSON through `views.<id>`; `sources.<id>` continues to expose flat
row arrays. The bindings below assume numeric `protein`, `fat`, `carbs`, `kcal`
and `target_kcal` fields in the actual dataset:

```md
::u-chart{type="line" :data="views.week" index="date" :categories='["protein","fat","carbs"]'}
::

::u-progress{:model-value="views.latest.kcal" :max="views.latest.target_kcal"}
::
```

Each jq filter must contain 1–8192 bytes and produce exactly one JSON value;
wrap stream results in an array when needed. A missing latest record may return
null, which displays “Нет данных” for bound components. Unknown fields in
nonempty data produce an explicit error. Chart series require numeric values
or explicit null gaps; progress values must be nonnegative and maxima positive.
These component checks run before mounting, including bindings resolved in slots.

View computation runs in a disposable process using Bashkit jq and an isolated
in-memory filesystem. It receives only the selected input rows, with no inherited
host environment, filesystem mounts or network access. Per document, input and
output are bounded to 512 KiB, wall time to three seconds, CPU time to two seconds,
and resident memory is monitored against 256 MiB. At most four workers run per
gateway process; excess computations return a retryable busy error. JSON nesting is limited to 16,
arrays to 1000 items and objects to 100 fields. Errors prevent publication.
Private pages recompute views from current sources; chat and public pages retain
their published results. Public pages expose no computation endpoint.

Views reject query results with `next_cursor`: computing totals or selecting the
latest row from a truncated source would be misleading. Narrow the query, raise
its bounded limit where appropriate, or use a grouped `summarize` source first.

## Components

### Table headers and date display

`data-table` accepts `columns` as JSON. Columns select/order fields and set labels;
omit the property to retain automatic columns. Each definition has `accessorKey`,
optional `header` and optional `dateFormat` (up to 34 distinct columns):

```md
::data-table{source="days" :columns='[{"accessorKey":"date","header":"День","dateFormat":"short"},{"accessorKey":"kcal","header":"Калории, ккал"}]'}
::
```

Native `u-table` supports the same `dateFormat` extension in its existing column
definitions. Formatting changes cell display only: sorting uses the
original ISO value, and filtering/pagination retain source values and settings.
A named `#date-cell` slot takes precedence over the column formatter.

The reusable `date-value` block works in cards, text, table cells and other named
slots. It accepts a literal ISO `value` or a colon binding to sources/views/slots:

```md
Дата: :date-value{:value="views.latest.date" date-format="long"}.

::u-table{:data="sources.days" :columns='[{"accessorKey":"date","header":"Дата"}]'}
#date-cell
:date-value{:value="slot.row.original.date" :date-format='{"day":"2-digit","month":"long","year":"numeric"}'}
::
```

Both use the same `dateFormat`: a `short`/`medium`/`long`/`full` date style, or a
JSON object of `Intl.DateTimeFormat` settings. Supported keys are `locale`,
`timeZone`, `dateStyle`, `timeStyle`, `year`, `month`, `day`, `weekday`, `hour`,
`minute`, `second`, `hour12`. Defaults are `ru-RU`, UTC, and short date style when
no date/time fields are specified. For example, `{"month":"long","year":"numeric"}`
displays `октябрь 2026 г.`; `{"timeStyle":"short","timeZone":"Europe/Moscow"}`
displays a timestamp's local clock time. Intl style options cannot be combined
with individual date/time fields. Unsupported locales/timezones/options fail.

Calendar dates (`2026-10-21`) retain their day in every timezone. Timestamp strings
must include `Z` or an explicit offset (e.g. `2026-10-21T23:30:00Z`); these convert
to the selected zone. Null/missing values show “Нет данных”. Impossible dates and
ambiguous local timestamps produce an error. Raw `{{ ... }}` bindings keep their
original text, and no automatic date detection changes existing documents.

`u-progress` also supports an MDC `segments` adapter for a single stacked bar:

```md
::u-progress{:max="100" :segments='[{"value":80,"color":"primary","label":"Основное"},{"value":20,"color":"error","label":"Превышение"}]' aria-label="Белки" status}
::
```

This displays 80% in the primary color and 20% in the error color inside the same
100% track. Values are portions in the same units as `max` (default 100), not
percentages unless the scale is 100. The sum must not exceed `max`; unused space
remains empty. Up to 32 nonnegative numeric portions use theme colors
`primary`, `secondary`, `success`, `info`, `warning`, `error`, or `neutral`.
Optional labels are included in accessible value text. Use `segments` instead
of `modelValue`; normal progress bars retain the native component.
`:segments="views.portions"` binds computed portions from dataset data. The
adapter does not infer which portion represents excess; define that in the view.

Normal Markdown supports headings, paragraphs, lists, emphasis, links, code and
tables. MDC blocks open with `::name{property="literal"}` and close with `::`
on a new line. Nested blocks use `:::`. YAML properties inside an MDC block also
work; whole-document frontmatter and component remapping do not.

`ui_components({"component":null})` lists available tags; pass `u-chart` or
`UChart` to inspect its package API, props, slots and adapter rules. Build/dev/
typecheck generate the client manifest, fixed lazy imports and Rust catalog
from installed `@nuxt/ui` 4.12.0. Checked-in artifacts allow Rust-only builds;
the catalog test detects stale artifacts after a package update.

The curated set excludes Layout (including Page and Dashboard), AI Chat,
Editor and its helpers, application navigation/providers, marketing/publishing,
AuthForm and FileUpload. Existing `columns` remains an application-owned helper.
Use catalog `::u-*` tags with JSON props, e.g. `:items='["One","Two"]'`;
`:data="sources.meals"` reads normalized source rows. `#header`, `#footer` and
other named slots can read `slot.item.label` or `slot.row.original.field` through
colon props or `{{ ... }}` text. Paths read own properties only; they do not
evaluate expressions. Date/time controls accept ISO strings. Standard controls
retain local model state; native component function props require a host adapter.

For a range corridor, use the UChart `range` adapter:

```md
::u-chart{type="line" :data="views.week" index="date" :range='{"min":"kcal_min","max":"kcal_max"}' :categories='[{"key":"target_kcal","label":"Ориентир","color":"warning"}]' aria-label="Коридор калорий"}
::
```

The translucent solid area spans the two bounds; its line is the midpoint
`(min + max) / 2` of each row, not a period average. No boundary lines are drawn.
`categories` is optional and adds reference lines; categories naming either
bound are omitted automatically. Use unique string/numeric `index` values and
numeric bounds with min ≤ max; an explicit null bound leaves a gap. Missing
fields and invalid bounds fail validation. The adapter supports straight line
charts, without stacking, smoothing or custom `definition`. Hover and keyboard
tooltips show bounds, midpoint and references; a custom `#tooltip` is preserved.
Native charts without `range` keep their existing behavior.

Existing shorthand blocks remain supported:

| Block | Properties and behavior |
| --- | --- |
| `columns` | Responsive Nuxt UI grid containing nested blocks; optional `count` (1–4) sets desktop columns, default 3 |
| `card` | Optional `title`, `description`; Markdown children |
| `metric` | Required `label`; literal `value`, or `source` and `field`; optional `unit` |
| `data-table` | Required `source`; optional `title`; filter/sort displayed rows |
| `line-chart` | Required `source`, `x`, `y`, `label`; optional `unit`; UChart line with tooltips and accessible data list |
| `callout` | Optional `title`, `color`; Markdown children; colors info/success/warning/error/primary/neutral |
| `button` | Required `label`, `prompt`; fills the existing chat composer for review |

Dataset record sources expose their values as table columns, plus record ID.
Summary sources expose `group`, record `count` and each numeric field's sum.
An ungrouped summary is suitable for a total metric; day/month groups suit
charts. A metric reads the first source row. Missing values show “Нет данных”.
The line chart uses evenly spaced source rows on its horizontal axis; use a
sorted day/month summary for a time series. Its visible data list preserves
access to the actual labels and numeric values.

Buttons do not call arbitrary tools, mutate records or send messages on their
own. On standalone pages they are inactive; use the page's Open chat link to
continue. `::u-button{prompt="..."}` has the same reviewed-chat behavior. Arbitrary
JavaScript, event handlers, executable Vue directives and unlisted components
are unsupported. JSON class/ui props are available; prefer standard defaults.
The complete parsed AST is checked before
mounting and rendered with fixed component references. Parser-stripped unsafe
attributes stay absent; remaining unsupported syntax produces an error alert.

## Forms and dataset actions

`show_ui.actions` defaults to `[]` for existing documents. An action declares
`id`, `dataset`, `operation` (`insert`/`update`), writable `fields`, and
`record_id` (null for insert, fixed existing ID for update). Up to eight actions
are allowed. Dataset schemas are authoritative; insert actions cover all required
fields. Snapshots resolve field metadata and current update values/revision;
only declared fields are included in the action's value snapshot.

Bind `::u-form{submit-action="add_meal"}` to an action ID and put named controls
inside `::u-form-field{name="meal" label="Meal"}`; a `::u-button{type="submit"}`
submits. See the bundled web-pages skill for a complete example. Plain forms
without actions remain local. Chat and public action forms are disabled; private
published pages receive action URLs pinned to the release.

`POST /api/documents/<slug>/actions/<id>?release=<UUID>` requires owner login,
same-origin Origin and the current private document release. The body is bounded
to 16 KiB: `request_id` UUID, flat `values` object and `expected_revision` (null
for insert, current record revision for update), plus optional `clear_fields`
(default `[]`) to unset declared optional fields on updates. Required fields
cannot be cleared. Dataset/operation/record target
come from the published action, never from client input. Writes reuse DatasetStore
schema validation, receipts and revision checks; other existing fields survive
updates. Insert retries reuse the request UUID and record ID. Validation errors
and conflicts are shown in the form; successful saves reload document sources.
No LLM call, generic database endpoint or deletion action is involved.

## Chat snapshots and live pages

`show_ui` resolves bounded source reads and returns a `ui_document` tool result.
The existing chat history stores that exact document and data. Reloading chat
does not replace old totals with current dataset values. A truncated table is
clearly marked as a saved slice; it does not fetch historical pages that no
longer have a matching dataset revision.

With a slug, the tool publishes source definitions and a snapshot atomically.
New pages default to private; null visibility preserves the previous mode.
Private pages use current owner dataset values when loaded/refreshed and have
authenticated pagination tied to the named published query and release. A
changed dataset invalidates the cursor, and the table offers restarting from
the first page. Sorting/search affects the current page only.

Explicitly public pages show only their published snapshot. They contain no live
source URLs, and their source API refuses access even for a logged-in owner.
Publishing again refreshes the public snapshot. Changing a page back to private
cannot retract copies already downloaded.

The tool renders chat-only documents without web publication enabled and in
isolated conversation spaces, using only that space's dataset tool. Publishing
requires the owner's web configuration. Legacy HTML pages continue to use
`publish_page`; existing pages are not converted automatically.

## Limits and operation

- Title: 1–200 characters; body: at most 32 KiB.
- At most eight sources; each inline source: 200 rows and 64 KiB.
- Inline values are bounded scalars; strings: at most 2048 UTF-8 bytes.
- Complete resolved snapshot: at most 512 KiB; dataset query/summary limits apply.
- Parsed tree: at most 5000 nodes and 24 nested levels; known literal props only.

The renderer uses Nuxt UI's UChart with bundled `@tanstack/charts@1.1.0`, without
CDN scripts. Components load on demand; shared Nuxt UI imports remain shared.
Rebuild the web bundle and Kodrik binary, then restart gateway to deploy changes.
Native document releases, manifests and backups share the existing page store;
see [Web pages](web-pages.md) and [Storage](storage.md).

Tool descriptions, the default prompt and the web-pages skill direct new pages
and follow-up UI requests through `show_ui`, including requests for interactivity.
Unsupported interactions are explained with a supported MDC alternative;
they do not trigger an HTML fallback or automatic renderer changes. The HTML
workflow is a separate reference for maintaining existing custom pages.
Native filesystem permissions remain available for development
work as requested; no new source-access sandbox is introduced. A new component
type still requires application development and a rebuild.
