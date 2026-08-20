# Updating the Docs

The documentation is built with [MkDocs](https://www.mkdocs.org/) and the
[Material](https://squidfunk.github.io/mkdocs-material/) theme, with API
reference pages rendered from docstrings by
[mkdocstrings](https://mkdocstrings.github.io/). The site is deployed to
GitHub Pages from CI on every push to `main` that touches the docs.

## Layout

- `mkdocs.yml` — site configuration and navigation
- `docs/` — all pages (Markdown)
- `docs/api/` — mkdocstrings stubs for the Python provider packages
- `docs/requirements.txt` — the doc-build dependencies
- `.github/workflows/docs.yml` — build + deploy pipeline

## Local preview

```bash
pip install -r docs/requirements.txt
mkdocs serve        # http://127.0.0.1:8000, live-reloads on save
```

Before opening a pull request, verify the strict build passes — CI runs the
same command, and warnings are errors there:

```bash
mkdocs build --strict
```

## Adding a page

1. Write the Markdown file under `docs/`.
2. Add it to the `nav:` section of `mkdocs.yml` in the appropriate section.
3. Run `mkdocs build --strict` — it catches broken internal links.

Internal links use relative Markdown paths (e.g. `[Slurm](slurm.md)`), so
they work both on GitHub and on the rendered site.

## API reference pages

Each `docs/api/<package>.md` page is a thin stub containing a
`::: <module>` directive; mkdocstrings reads the source under
`providers/src` (see the `paths` option in `mkdocs.yml`) and renders the
docstrings. Improving a provider's docstrings therefore improves the
published API reference with no doc edits at all. Docstrings use the Google
style.

## What CI does

`.github/workflows/docs.yml` triggers on pushes to `main` that touch
`docs/`, `mkdocs.yml`, or the provider sources. It installs
`docs/requirements.txt` plus the providers package, runs
`mkdocs build --strict`, uploads `site/` as the Pages artifact, and deploys
it. There is no `gh-pages` branch to manage.
