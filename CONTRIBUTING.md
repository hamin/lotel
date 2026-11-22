# Contributing

Thanks for helping. Bug reports, docs fixes and pull requests are all welcome.

## Before you start

- **Bugs:** open an issue with what you ran, what you expected and what
  happened. `lotel --version` and your OS help.
- **Bigger changes:** open an issue first so we can agree on the shape before
  you spend time on it.
- **Security issues:** see [SECURITY.md](SECURITY.md). Please don't file them
  as public issues.

## Building and testing

The [Development](README.md#development) section of the README has the setup.
Before you open a pull request, run:

```sh
just fetch-extensions
cargo test
cargo clippy --all-targets -- -D warnings
just smoke
```

CI runs the same checks.

## A few house rules

- **UI styling** comes from `design/useby`. Change it there and regenerate
  `src/skin.rs` as described in [Changing the look](README.md#changing-the-look),
  rather than editing the generated file.
- **Test fixtures** must not contain real prompts, tool content, emails, home
  paths, host names or account IDs. Redact them before you commit; see the
  `redaction` list in `testdata/agent/*-meta.json`.
- **Docs** live in `docs/` and are published at
  [lotel.io/docs](https://lotel.io/docs/). Update them when you change
  behaviour.
- Add a line to [CHANGELOG.md](CHANGELOG.md) under the unreleased version.

## License

By contributing you agree that your work is licensed under the project's
[Apache-2.0 license](LICENSE).
