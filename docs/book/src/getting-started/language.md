# Language & translations

ZeroClaw's interface strings (CLI messages and command help) can be shown in
Simplified Chinese. English and Simplified Chinese catalogues ship with the
binary. CLI strings are available immediately; translated tool descriptions
can be installed from the bundled catalogues.

> Retired locales: `es`, `fr`, and `ja` were removed from the selectable
> registry. If your config still selects one, ZeroClaw logs a fallback notice
> and renders in English, except for catalogues you fetched before retirement:
> those on-disk files keep overriding ordinary messages. Fork-specific updater,
> locale-installation, and Telegram command guidance always uses the current
> build's catalogues. See
> [Troubleshooting](#troubleshooting) below.

## Set your language

ZeroClaw reads a top-level `locale` key from your config. Set it to `en` or
`zh-CN`. If `locale` is unset, ZeroClaw uses your operating system's language
and falls back to English when no translation is available.

## Install your language files

For Simplified Chinese tool descriptions, install the bundled files once:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw locales fetch zh-CN
```

</div>

The command retains the name `fetch` for compatibility, but restores the exact
catalogues shipped with the running binary without network access. It installs
them under `~/.zeroclaw/data/ftl/zh-CN/`, where ZeroClaw looks for them at startup.
Restart ZeroClaw afterward to pick them up. Run it again after a source upgrade
to replace older files with that build's catalogues.

### Installing only part of a language

By default `fetch` restores every catalogue for the locale. To restore only
some, pass `--catalog` with a comma-separated list:

| Catalog | Covers |
|---|---|
| `cli` | CLI messages and command help |
| `tools` | Built-in tool descriptions |

<div class="os-tabs-src">

#### sh

```sh
zeroclaw locales fetch zh-CN --catalog tools   # just tool descriptions
```

</div>

Both supported locales (`en` and `zh-CN`) include every selectable catalogue.
Unknown locale or catalogue names are rejected.

## Where the files live

| Path | What |
|---|---|
| `~/.zeroclaw/data/ftl/<locale>/cli.ftl` | CLI message translations |
| `~/.zeroclaw/data/ftl/<locale>/tools.ftl` | Tool description translations |

If you run ZeroClaw with a custom config directory (`--config-dir` or
`ZEROCLAW_CONFIG_DIR`), the files install under that directory's `data/ftl/`
instead.

## Troubleshooting

- **Still seeing English after fetching.** Confirm `locale` in your config
  matches the locale you fetched, and restart the process. ZeroClaw loads
  language files at startup.
- **Config selects `es`, `fr`, or `ja`.** Those locales are retired from the
  registry: `zeroclaw locales fetch` rejects them, and without on-disk
  catalogues every string renders in English with a fallback notice in the
  log. Catalogues fetched before retirement under `data/ftl/<locale>/` keep
  translating ordinary messages. Fork-specific updater, locale-installation,
  and Telegram command guidance falls back to the current embedded English
  text so an old catalogue cannot advertise a retired operation.
- **Update help still describes upstream releases in an old installation.**
  Upgrade the fork from source, then run `zeroclaw locales fetch zh-CN` to
  restore its matching catalogues. The current binary keeps its source-only
  update guidance even when an older disk catalogue is present.
- **A specific string is in English even though the rest is translated.** That
  individual string has no translation yet and falls back to English by design.
