# Documentation conventions

The `docs/` Markdown files are the documentation source. An HTML generator and
hosting system have not been selected. Keep the content independent of either
so it can be published later without rewriting the guides.

## Structure and navigation

- Keep one topic per page and one H1 per page, with descriptive H2 sections.
- Use relative Markdown links between pages.
- Use stable filenames; preserve existing links when reorganizing content.
- Add new pages to the task table in the [documentation index](index.md).
- Keep configuration defaults in `configuration.md`; link to them elsewhere.
- Give code fences a language and use placeholders rather than real credentials.

The root README is a short repository entry point; detailed guides live here.
Ignored working plans under `docs/superpowers/` are not maintained product
documentation and should be excluded from any future publication.

## Checks before committing

Check descriptions against the source, verify relative links and section anchors,
and validate configuration examples. Avoid generator-specific directives and
plugins until a publishing system is chosen.
