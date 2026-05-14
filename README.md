# rseek

rseek is a Rust CLI for crawling web pages and searching the crawled corpus with BM25 ranking. Crawling stays on the seed URL's origin by default, persists fetched pages to a JSONL store, and `search` rebuilds an index from that store on demand.

## Status

`rseek search` requires a prior `rseek crawl`; the search index is not a long-running service or prebuilt database. Pages are stored as JSONL, then indexed on demand when you run `search`.

Default store path:

- Windows: `%LOCALAPPDATA%\rseek\pages.jsonl`
- Linux: `$XDG_DATA_HOME/rseek/pages.jsonl`, or `~/.local/share/rseek/pages.jsonl` when `XDG_DATA_HOME` is unset
- macOS: `~/Library/Application Support/rseek/pages.jsonl`

Requires Rust 1.74+.

## Install

Install directly from GitHub:

```bash
cargo install --git https://github.com/arazmj/rseek
```

Or build from a clone:

```bash
git clone https://github.com/arazmj/rseek.git
cd rseek
cargo build --release
```

The local binary is written to `target/release/rseek`.

## Quick start

```bash
rseek crawl https://example.com --max-pages 10
rseek search "domain"
rseek search "example domain"
```

Expected output shape:

```text
https://example.com/ · Example Domain · 1.23
```

Each result prints `URL · title · score`.

## `crawl` reference

```bash
rseek crawl <url> [options]
```

- `url` positional: seed URL to crawl.
- `--concurrency`, `-c` (default: `10`): maximum concurrent fetches.
- `--max-pages`, `-m` (default: `100`): maximum pages to store before exiting.
- `--timeout`, `-t` (default: `10s`): HTTP request timeout.
- `--allow-cross-origin` (default: off): allow crawling links outside the seed URL's host.
- `--store`, `-s` (default: platform data path above): JSONL page store.

By default, the crawler follows normalized HTTP(S) links on the same host as the seed URL.

## `search` reference

```bash
rseek search <query> [options]
```

- `query` positional: search terms to rank against stored pages.
- `--store`, `-s` (default: platform data path above): JSONL page store to read.

Search is case-insensitive and punctuation-aware.

### Logging

RSeek uses structured tracing logs for crawler activity. Set `RUST_LOG=debug` to include detailed crawl and fetch diagnostics, or omit it to use the default `info` level.

## How it works

The crawl loop fetches pages with bounded concurrency using `tokio::sync::Semaphore`, extracts links, normalizes URLs, and stops once `--max-pages` is reached or no more eligible URLs remain. HTTP requests use a User-Agent, status checks, and the configured timeout.

Fetched pages are appended to a JSONL store so crawl and search are decoupled. Each `search` command reads that store, rebuilds an in-memory index, tokenizes text case-insensitively, and ranks results with BM25 via `probly-search`.

## Project structure

- `src/main.rs` - CLI entry point and command wiring.
- `src/page.rs` - page model and HTML extraction.
- `src/store.rs` - JSONL persistence.
- `src/tokenizer.rs` - search tokenization.

## License

MIT.

## Contributing

Contributions are welcome. Please open an issue or pull request at https://github.com/arazmj/rseek/issues with bugs, feature requests, or implementation notes.
