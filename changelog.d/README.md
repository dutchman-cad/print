# Changelog fragments

For each change, add `changelog.d/<short-slug>.md` instead of editing `CHANGELOG.md`.
Use a distinct, descriptive filename so parallel pull requests can add changes independently.
The first line is `### <Heading>`, followed by a blank line and one or more `- ` bullet items.
Wrap at about 95 columns, indent continuations by two spaces, and use Oxford commas.
For example:

```markdown
### History

- Simulated printers report job progress.
```

The allowed headings and their order are defined by `HEADINGS` in
[`scripts/changelog.py`](../scripts/changelog.py). Use `Docs` for documentation and release
workflow changes. Known issues belong in the dated release entry when preparing a release.
`README.md` is documentation, not a fragment.

Run `python3 scripts/changelog.py check` to validate fragments and catch repeated topic
headings in any released version. Run `python3 scripts/changelog.py preview` to print the
would-be Unreleased section: topics follow the canonical order, and fragments within each
topic follow filename order. Empty topics are omitted; bullet order within a fragment is kept.

Run `python3 scripts/changelog.py release X.Y.Z` to print the dated version section without
changing files. `--date YYYY-MM-DD` sets its date (otherwise today's local date is used).
If the version is already in `CHANGELOG.md`, the command prints that section instead of
including newer fragments. With `--write`, it inserts a new section into `CHANGELOG.md`
and deletes the included fragments, leaving this README; an existing version is refused.
See [Releasing](../docs/print.md#releasing) for draft builds and the hardware testing rule.
