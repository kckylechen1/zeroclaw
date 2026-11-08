# Language & translations

ZeroClaw's interface strings (CLI messages and command help) can be shown in
Simplified Chinese. English is always built in; Simplified Chinese CLI strings
are also embedded, and translated tool descriptions are downloaded on demand.

> Retired locales: `es`, `fr`, and `ja` were removed from the selectable
> registry. If your config still selects one, ZeroClaw logs a fallback notice
> and renders in English, except for catalogues you fetched before retirement:
> those on-disk files keep overriding as before. See
> [Troubleshooting](#troubleshooting) below.

## Set your language

ZeroClaw reads a top-level `locale` key from your config. Set it to `en` or
`zh-CN`. If `locale` is unset, ZeroClaw uses your operating system's language
and falls back to English when no translation is available.

## Fetch your language files

English ships inside the binary. For Simplified Chinese tool descriptions you
fetch the translated files once:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw locales fetch zh-CN
```

</div>

This downloads the Simplified Chinese translation files from the ZeroClaw
project and installs them under `~/.zeroclaw/data/ftl/zh-CN/`, where ZeroClaw
looks for them at startup. Restart ZeroClaw afterward to pick them up.

### Fetching only part of a language

By default `fetch` downloads every catalogue for the locale. To download only
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

If a catalogue has not been translated for your language yet, `fetch` skips it
and tells you: the catalogues that do exist are still installed.

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
  working unchanged.
- **`fetch` reports a catalogue was skipped.** That catalogue has not been
  translated for your locale yet. The available catalogues are still installed;
  untranslated strings fall back to English.
- **A specific string is in English even though the rest is translated.** That
  individual string has no translation yet and falls back to English by design.
