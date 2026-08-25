# Stream Lib

> **Vendored, patched copy — otherwise pristine upstream 0.5.2.** apollo
> pins this crate via `[patch.crates-io]` in the workspace `Cargo.toml` and
> applies `../stream_lib-hls-leak.patch` (`src/hls/mod.rs` only) on top of
> it. Upstream's HLS `bytes_forwarder`/`download_to_file` never check
> whether the downstream consumer is still listening: when songbird drops
> its reader on track skip/error, they keep polling the live playlist and
> downloading segments — for a 24/7 stream, indefinitely — logging a
> `warn!` on every send into the dead channel. The patch adds an
> `event_tx.is_closed()` check to both loops so they unwind promptly
> instead. Confirmed still present on upstream's `master` as of 2026-08-25.
>
> **To bump to a newer upstream release:**
> 1. Replace this directory's contents with the new version (e.g. `cp -r
>    ~/.cargo/registry/src/*/stream_lib-<ver> vendor/stream_lib`, then
>    restore this README's patch note and swap in the fresh `Cargo.toml`
>    from `Cargo.toml.orig`, dropping the auto-generated header — see the
>    original registry copy for the pattern).
> 2. Check whether upstream fixed the leak (search `src/hls/mod.rs` for
>    `bytes_forwarder`); if so, drop `stream_lib-hls-leak.patch` and this
>    note entirely.
> 3. Otherwise reapply it: `patch -p1 -d vendor/stream_lib <
>    vendor/stream_lib-hls-leak.patch` (falls back to a manual re-edit if
>    upstream has since changed the surrounding lines — the patch is 15
>    lines, two `if event_tx.is_closed() { break; }` checks).
> 4. `cargo build` to confirm it still compiles against songbird's expected
>    API.

This library makes it possible to download various types of video streams.
Currently it supports HLS and chunked http streams.

## Example

```rust
use futures_util::StreamExt as _;
use reqwest::Client;
use stream_lib::Event;
use tokio::io::AsyncWriteExt;

/// Write buffer
pub const WRITE_SIZE: usize = 131_072;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = std::env::args().collect::<Vec<_>>();
    let url = args.get(1).expect("Pass a url as the first argument");

    let http = Client::new();
    let req = http.get(url).build()?;
    let mut dl = stream_lib::download_hls(http, req, None);

    let mut file = tokio::io::BufWriter::with_capacity(
        WRITE_SIZE,
        tokio::fs::File::create("./example.mp4").await?,
    );

    while let Some(event) = dl.next().await {
        match event {
            Event::Bytes { bytes } => {
                file.write_all(&bytes).await?;
            }
            Event::End => break,
            Event::Error { error } => {
                eprintln!("Encounted error: {}", error);
                break;
            }
        }
    }
    Ok(())
}
```
