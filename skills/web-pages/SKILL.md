---
name: web-pages
description: Use when the user wants a page, dashboard, report, interactive table or visualization in Kodrik web chat, or wants to publish or update a persistent page.
---

# Web Pages

## Choose the content tool

For new pages, dashboards, reports and rich chat responses, use `show_ui`.
It renders Markdown/MDC with the host's Nuxt UI components and
Catppuccin Blue theme. Prepare content and source queries through tools;
ordinary page creation does not require editing the web application's code,
installing packages or preparing HTML files.

- Chat-only response: `show_ui` with `slug: null`, `visibility: null`.
- New persistent page: use a stable slug; new pages default to private.
- Update a native MDC page: call `show_ui` with its existing slug and
  `visibility: null` to preserve access.
- Update an existing custom HTML page: follow the legacy workflow below,
  preserving its design and data. Migrate formats only when requested.

### Follow-ups and catalog limits

Continue using `show_ui` for follow-ups to native content, including
"make it interactive", "add filters" or "make it a page". Publishing uses
the same MDC body and sources with a slug; it does not require HTML files.
Preserve the slug and visibility when updating a native page.

Discover the installed component set with `ui_components(component: null)`;
request a component tag/name for its props, slots and adapter rules before
using it. The package-derived catalog excludes Layout, AI Chat, Editor,
app navigation/providers, marketing/publishing, AuthForm and FileUpload.
Use standard component defaults with minimal class/ui overrides.

Nuxt UI controls retain local state; tabs, selects, inputs, overlays and
accordions work in the document. UChart and the `line-chart` alias provide
hover/keyboard tooltips. Period filtering needs explicit views/data (for
example named tab slots with bounded series), not an invented chart prop.
Chat prompt buttons prepare a follow-up for review. If an interaction is
missing, explain that specific limit and offer an MDC alternative, such as
a chart and filterable data table or a bounded date-range follow-up in chat.
Do not promise unsupported interactions or silently replace them with
HTML/JavaScript, Chart.js, another framework or edits to the host app.
Extending the renderer requires a separate user request for development.
If `show_ui` is unavailable, explain that; do not create an HTML fallback.

### Data and MDC catalog

Keep durable diary/tracker entries in `datasets`. Query bounded date ranges
and use `summarize` for totals or chart series. A source has a unique `id` and
exactly one of `query` or `data_json`. Query uses `mode: "query"` or
`"summarize"`, `dataset`, and the corresponding dataset filter/date/metric/
group/limit parameters. Set unused fields to null. Inline `data_json` is a
JSON array of flat scalar rows for small supplied data, not a replacement for
the durable diary. Do not invent missing values; use explicit unknown values.

MDC uses literal properties and closing delimiters. Supported blocks:

All components returned by `ui_components` use their `::u-*` tag. Supply
arrays/objects/booleans/numbers as JSON: `:items='["One","Two"]'`. Reference
resolved source rows with `:data="sources.meals"`. Named slots use `#header`,
`#footer`, `#item` etc.; read scoped values through `slot`, such as
`:label="slot.item.label"` or `{{ slot.row.original.meal }}`. Only own-property
paths are supported, not expressions, function calls or event handlers.
Date/time component values use ISO strings. Existing shorthand blocks remain:

| Block | Properties |
| --- | --- |
| `columns` | Responsive group; `count="4"` puts four cards in a desktop row (two on tablets, one on phones). Optional count 1–4, default 3; nested blocks use `:::` |
| `card` | Optional `title`, `description`; Markdown children |
| `metric` | `label`, literal `value` or `source` + `field`; optional `unit` |
| `data-table` | `source`, optional `title`; built-in filtering and sorting |
| `line-chart` | `source`, `x`, `y`, `label`, optional `unit` |
| `callout` | Optional `title`, `color` (info/success/warning/error/primary/neutral); Markdown children |
| `button` | `label`, `prompt`; fills chat composer for user review |

For example, after reading the real nutrition dataset, pass body:

```md
# Nutrition

::metric{label="Calories" source="totals" field="kcal" unit="kcal"}
::

::data-table{source="meals" title="Meal history"}
::
```

Bind `totals` to a `summarize` query with `metrics: ["kcal"]`, and `meals`
to a bounded `query`, both using the actual dataset name. Summary rows expose
`group`, matching-record `count` and each metric's sum by its field name.
Use `group_by: "day"` for a daily series and `x="group"`, `y="kcal"` in a
line chart. Omit grouping for a total metric. Table filters/sorting apply to
the displayed page; private persistent pages can load subsequent pages.

For rolling periods, set source query `last_days` (1–366) and the user's IANA
`timezone` (e.g. `Europe/Moscow`); leave absolute date bounds null. This includes
today and resolves again on each private page load. UTC is the default timezone.

For computed data, declare `views: [{id, source, jq}]`. jq receives the complete
flat row array from the named source and must return one JSON value. For example,
`{"id":"latest","source":"days","jq":"sort_by(.date) | last"}` selects the
latest day; `{"id":"week","source":"days","jq":"sort_by(.date)"}` sorts a
series. Bind `:data="views.week"` on UChart with multiple numeric categories,
and `:model-value="views.latest.kcal" :max="views.latest.target_kcal"` on
UProgress. For a stacked bar use `:segments="views.portions"` instead of
`modelValue`, with `{value, color, label?}` portions and a numeric `max` (default
100). For example, values 80/20 and colors primary/error fill one 100-unit track.
Values are in the same units as max; their sum must not exceed max. Unfilled
space stays empty. Use up to 32 nonnegative values and semantic theme colors;
compute the meaning of each portion in the view. Use the real dataset field
names. Missing data is not zero; an empty
latest view may return null. Query sources must be complete: narrow a paginated
query or summarize it before computing a view. Source paths are already flat
rows; do not add `.records` or `.values` to MDC bindings. jq runs in an isolated,
resource-limited process; filters cannot access host files or the network.
Views allow data transformations, not JavaScript, handlers or HTML. Invalid
view results prevent publication; chart/progress binding shapes are checked
before display. For the complete limits, read the tool's API.

For a min/max corridor use UChart with
`:range='{"min":"kcal_min","max":"kcal_max"}'`, `index="date"` and
`:data="views.week"`. The host draws a solid translucent band between the bounds
and a line at each row's `(min + max) / 2`; no jq midpoint is needed. This is not
a period average. Optional `categories` adds reference lines, such as a target;
categories naming either bound are omitted. Bounds must be numbers with min ≤
max; explicit null creates a gap. Use unique index values, straight line charts,
and no stacked/smoothing/custom definition. Read `ui_components("u-chart")` for
the exact adapter contract. Tooltips show the corridor and midpoint.

### Table headers and dates

`data-table` accepts `:columns='[{"accessorKey":"date","header":"Дата","dateFormat":"short"}]'`
to choose/order columns, set human labels and format date cells. Omit columns for
the old automatic table. Native `u-table` columns accept the same `dateFormat`
extension; raw accessors still drive sorting and filtering.

For text/cards/scoped cells use `:date-value{:value="views.latest.date" date-format="long"}`
or `:date-value{:value="slot.row.original.date" :date-format='{"month":"long","year":"numeric"}'}`.
Both accept short/medium/long/full styles or Intl date options (locale/timeZone,
dateStyle/timeStyle or year/month/day/weekday/hour/minute/second/hour12). Default
ru-RU/UTC. Calendar dates retain their day; timestamps require an explicit offset
and convert to the requested timezone. Null shows Нет данных; invalid dates/options
fail. Do not invent pattern strings or put formatting functions/expressions in MDC.

### Forms that save records

Inspect the actual dataset schema first. Declare `actions` in `show_ui`:

```json
[{"id":"add_meal","dataset":"nutrition","operation":"insert","fields":["date","meal","kcal"],"record_id":null}]
```

Include all required fields for insert. For update use `operation: "update"`
and the existing record ID; only declared fields change and other fields are
preserved. Bind the action to native UForm and give each field its dataset name:

```md
::u-form{submit-action="add_meal"}
:::u-form-field{label="Date" name="date"}
::::u-input{type="date"}
::::
:::
:::u-form-field{label="Meal" name="meal"}
::::u-input
::::
:::
:::u-form-field{label="Calories" name="kcal"}
::::u-input-number
::::
:::
:::u-button{type="submit" label="Save"}
:::
::
```

Use the actual dataset/field names, not these example names blindly. A private
persistent page enables saves through the gateway without invoking the agent;
chat-only and public forms cannot save. Update forms load current values and
use record revisions; conflicts show an error rather than overwriting changes.
Successful saves reload all related source views. Do not imply browser edits
are persisted without a declared action and private publication.

The renderer accepts the generated catalog, JSON props and bounded data paths.
Do not include HTML, JavaScript, event handlers, executable Vue directives or
component remapping. Functions require an application adapter; use only its
documented capabilities. Ordinary form submissions use declared dataset actions.
Body limit: 32 KiB; up to eight sources, inline data 200 rows/64 KiB per
source, complete snapshot 512 KiB. Narrow queries if limits are exceeded.
Chat results are frozen; private pages read current dataset values on load;
public pages show only the published snapshot. Public visibility requires
explicit user authorization. Standalone pages use Open chat to continue;
document buttons are active inside chat and never submit automatically.
Publication does not start the gateway. Return the actual URL from the tool.

## Existing custom HTML pages

Read [the legacy HTML workflow](references/legacy-html.md) only when the user
asks to maintain an existing custom HTML page. Its HTML/CDN examples are not
alternatives for new pages or unsupported native interactions. Preserve its
format, design and data; migrate it to MDC only when requested.
