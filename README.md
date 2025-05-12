# RSeek

RSeek is a powerful web crawler and search tool written in Rust. It allows you to crawl web pages and perform full-text search on the crawled content.

## Features

- Web crawling with configurable concurrency
- Full-text search using BM25 ranking algorithm
- HTML parsing and link extraction
- Support for both HTTP and HTTPS
- Concurrent request handling with Tokio
- Command-line interface with subcommands

## Installation

Since this is a Rust project, you'll need to have Rust and Cargo installed. You can install them from [rustup.rs](https://rustup.rs/).

To build the project:

```bash
cargo build --release
```

## Usage

RSeek provides two main commands:

### Crawl

Crawl a webpage and extract its content:

```bash
rseek crawl <url> [--concurrency <number>]
```

Options:
- `url`: The seed URL to start crawling from
- `--concurrency` or `-c`: Number of concurrent requests (default: 10)

Example:
```bash
rseek crawl https://example.com -c 20
```

### Search

Search through the crawled content:

```bash
rseek search <query>
```

Options:
- `query`: The search query to look for in the crawled content

Example:
```bash
rseek search "rust programming"
```

## Dependencies

- `hyper` - HTTP client and server
- `tokio` - Async runtime
- `scraper` - HTML parsing
- `probly-search` - Full-text search functionality
- `clap` - Command-line argument parsing
- `html_parser` - HTML parsing utilities
- `url` - URL parsing and manipulation

## Project Structure

- `src/main.rs` - Main application entry point
- `src/page.rs` - Page structure and parsing logic

## License

This project is open source and available under the MIT License.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request. 