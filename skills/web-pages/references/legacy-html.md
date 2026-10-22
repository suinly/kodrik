# Maintain existing HTML pages

Use this workflow only to maintain an existing custom HTML page. For new pages or follow-ups to native MDC content, return to `show_ui`; missing MDC interactions do not select this workflow.

Use `publish_page` only when available. Publication serves static files;
there is no browser write API. Enter and update records through conversation
with Kodrik. Do not add forms that promise durable writes from the browser.

Prepare files under the drafts root stated in the tool description, using
a stable slug of 1–64 lowercase ASCII letters, digits or hyphens. Include
`index.html`; keep source records in `data.json`, separate from HTML/CSS/JS.
Before updating, read existing data and preserve its history and format.
Write files atomically and validate JSON. Do not invent missing records or
macronutrients; show unknown values explicitly rather than treating them as zero.

Use relative local resources and `fetch('./data.json')` so proxy prefixes work.
Render imported text with `textContent`, not unchecked `innerHTML`. Do not
embed secrets. Public and private pages share the owner's browser origin;
CDN scripts execute with the same access as page code. Use the vetted,
version-pinned defaults below; additional libraries must meet the CDN rules
in this skill. Never load arbitrary scripts from imported content.

For a native dropdown, read [examples/select.html](../examples/select.html) before
styling it. For a time-series chart, read
[examples/rolling-chart.html](../examples/rolling-chart.html); adapt its data
shape and units rather than copying its example values. These are component
references, not a page template or a substitute for visual verification.

### Visual standard

Default to a calm, compact personal dashboard. An explicit user style or
existing user-approved design overrides this default; preserve its identity
when updating. Tailwind remains the styling foundation.

- Light: `stone-50` canvas, white surfaces, `stone-900` primary text,
  `stone-600` secondary text, `stone-200` borders, `emerald-700` accent.
- Dark: `zinc-950` canvas, `zinc-900` surfaces, `zinc-100` primary text,
  `zinc-400` secondary text, `zinc-700` borders, `emerald-400` accent.
- Use a system sans-serif stack, 16 px body text, medium-weight headings,
  large tabular numbers for totals and always-visible units. Use one accent
  consistently; distinguish chart series with labels and line patterns too.
- Center content in `max-w-6xl mx-auto px-4 sm:px-6 lg:px-8`; use a 4/8 px
  spacing rhythm, 16–24 px section gaps, subtle borders and `rounded-xl`.
  Keep shadows minimal. Avoid decorative gradients, glass effects, oversized
  marketing heroes and nested cards for ordinary logs and reports.
- Put the title, current period and theme selector first, followed by key
  totals, useful charts and detailed records. Adapt this hierarchy to the
  actual page; do not fabricate metrics or add charts without useful data.

### CDN libraries

Use Tailwind's browser runtime for these no-build pages. Do not create npm,
Vite, React or component-framework projects. Use the exact HTTPS URLs below;
do not use `latest`, unversioned URLs or Tailwind v3 configuration with v4.

```html
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<script src="https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4.3.3"></script>
<style type="text/tailwindcss">
  @custom-variant dark (&:where(.dark, .dark *));
</style>
<!-- Only on pages that need charts; load before the page's deferred app.js. -->
<script defer src="https://cdn.jsdelivr.net/npm/chart.js@4.5.1/dist/chart.umd.min.js"></script>
<script defer src="./app.js"></script>
```

Chart.js is optional, not a required dependency of every page. Use native
HTML controls and CSS for filters, tables and progress indicators. Do not
add another library unless it serves a requested capability and its official
browser distribution, exact version and HTTPS CDN URL have been verified.
Update a pinned version deliberately, preserving existing pages unless asked.

Tailwind Play CDN is a development-oriented browser runtime, chosen here
for the requested no-build workflow; do not promise offline styling. Include
a small native CSS fallback for readable text, basic spacing and theme
backgrounds. Show source data and summaries independently of Chart.js; if
the chart library fails to load, retain a table/list and a clear chart status
instead of crashing the page. Never send page records or credentials to a
CDN or analytics service. External requests fetch library code only.

### Light, dark and system themes

Every page supports all three modes with a visible, labeled native selector:
System / Light / Dark (translate labels to the page language). Default to
System. Persist only this preference in `localStorage` under `kodrik:theme`,
shared across pages; store no meal records or auth tokens there.

Initialize the theme in a small inline script in `<head>` before the CDN
script and first paint. Accept only `system`, `light` or `dark`; ignore invalid
stored values. Resolve System using `matchMedia('(prefers-color-scheme: dark)')`.
Toggle `.dark` on `<html>` and set its `style.colorScheme` to the resolved mode.
If storage is unavailable, keep an in-memory preference and continue working.

On selector changes, apply the theme immediately and save the choice when
possible. Listen for system preference changes while System is selected and
for `storage` events so other open pages stay consistent. Apply `dark:` variants
to every surface, text, border, input, focus state and empty/error state.
Keep chart axis labels, grids, legends and tooltips readable in both themes;
update existing charts on theme changes with `chart.update('none')` after
adjusting their colors. Do not recreate or reset the displayed data.

### Mobile and accessibility

Design from 320 px upward, then expand at `sm`/`md`/`lg` breakpoints. Use
`grid-cols-1 sm:grid-cols-2 lg:grid-cols-4` for comparable metrics when useful;
let labels and controls wrap. No page-wide horizontal scrolling, fixed desktop
widths, clipped totals or hover-only controls. Do not hide overflow to mask
a broken layout.

Use at least 44 px touch targets and visible keyboard focus. Keep long tables
in a labeled, keyboard-scrollable `overflow-x-auto` region, or show compact
record rows on phones; preserve access to every field. Place charts in a
width-constrained relative container with an explicit responsive height;
use Chart.js `responsive: true` and `maintainAspectRatio: false`. Provide a
text summary or table alongside canvas charts. Honor `prefers-reduced-motion`.

Before publication, check 320, 390, 768 and 1280 px widths; both resolved
themes; selector persistence/reload; System changes; keyboard controls;
empty and error states; and readable data when CDN loading fails. Use a
browser when available. If unavailable, report that visual checks were not
performed rather than claiming them. Verify data and JSON regardless.

### Publish

New pages default to private. Set `visibility: "public"` only on explicit
request for access without login; all files in the release become public.
On updates use `visibility: null` unless the user asks to change access.
Call `publish_page` after every data or presentation update. Return its
actual URL; publication does not start the gateway server. On failure,
report the error and preserve existing records. When the tool is unavailable,
explain that publishing requires enabling the web server; never invent a URL.

Example: add a meal to `nutrition/data.json`, preserving previous entries,
then call `publish_page({"slug":"nutrition","visibility":null})`.
An open page need not refresh automatically; current data loads on reopen.
