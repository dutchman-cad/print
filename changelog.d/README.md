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

To prepare a release, run `python3 scripts/changelog.py release X.Y.Z`, optionally with
`--date YYYY-MM-DD` (the default is today's local date). This inserts a dated entry above
the newest version and deletes the included fragments, leaving this README. Review the
entry, add any known issue, and stage the deletions with Git. An existing version is refused.
Then write the highlights in `release-notes/X.Y.Z.md`.
