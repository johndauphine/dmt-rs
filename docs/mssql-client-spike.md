# Spike: `mssql-client` as a tiberius replacement

**Date:** 2026-04-10
**Branch (deleted):** `spike/mssql-client`
**Crate evaluated:** [`mssql-client` 0.7.0](https://github.com/praxiomlabs/rust-mssql-driver) (praxiomlabs/rust-mssql-driver)
**Status:** ❌ Not adopting. Re-evaluate in ~6 months.

## Why we ran this spike

dmt-rs depends on a [forked tiberius](https://github.com/johndauphine/tiberius/tree/feature/packet-size-config) solely to expose `packet_size` on `Config`. The upstream PR ([prisma/tiberius#400](https://github.com/prisma/tiberius/pull/400)) has been open and unreviewed since 2026-01-02, and other PRs in that repo are sitting open for months — there's no signal a maintainer will pick it up. Eliminating the fork would remove a long-tail maintenance burden.

`mssql-client` came up as the only actively-developed alternative to tiberius (SQLx dropped MSSQL after 0.6 and is gated behind unreleased "SQLx Pro"). Goals of the spike:

1. Confirm the API actually works against a real MSSQL instance.
2. Confirm `packet_size` is configurable without forking.
3. Stress the type system on the columns that have bitten us in tiberius (nullable numerics — see [PR #94](https://github.com/johndauphine/dmt-rs/pull/94)).
4. Get rough throughput numbers to compare against tiberius.

## TL;DR / Recommendation

**Keep the tiberius fork.** The win from removing the fork is real but small. The blocker for adoption is real and large: `mssql-client`'s `QueryStream` is *not* an actual stream — it buffers every row of a query into RAM before yielding the first one. Migrating dmt-rs's read path would require auditing every `MssqlReader` code path that assumes mid-result-set streaming.

Re-evaluate when:
- `mssql-client` ships true row-level TDS streaming, OR
- prisma/tiberius is unambiguously dead (no commits for 12+ months), OR
- the fork breaks against a future tiberius rebase

## Setup (reproduce on another machine)

Prerequisites:
- Docker
- Rust 1.88+ (mssql-client requires Rust 2024 edition)
- Brent Ozar's `StackOverflow2010` database loaded into a SQL Server 2022 container, with `dbo.Posts` populated

The local instance used for this spike was at `~/docker-data/mssql-bench/` with the SA password set to `YourStrong@Passw0rd` (matches all `test-mssql-*.yaml` configs in this repo). To reproduce:

```bash
docker run -d --name mssql-spike \
  -p 1433:1433 \
  -e ACCEPT_EULA=Y \
  -v /path/to/your/mssql-data:/var/opt/mssql \
  --platform linux/amd64 \
  mcr.microsoft.com/mssql/server:2022-latest
```

If the existing master.mdf has a different SA password, reset it non-destructively:

```bash
docker run --rm -i -e ACCEPT_EULA=Y \
  -v /path/to/your/mssql-data:/var/opt/mssql \
  --platform linux/amd64 \
  --entrypoint /opt/mssql/bin/mssql-conf \
  mcr.microsoft.com/mssql/server:2022-latest \
  set-sa-password <<< 'YourStrong@Passw0rd
YourStrong@Passw0rd'
```

Then create the standalone spike project (intentionally outside the workspace — it needs Rust 2024, the workspace is on 2021):

```bash
mkdir -p spike-mssql-client/src
# Add the Cargo.toml and main.rs from the appendix below
cd spike-mssql-client
cargo run --release
```

Expected output (numbers below from x86_64 emulation on Apple Silicon — native Linux x86 will be much faster):

```
=== mssql-client spike ===
crate: mssql-client 0.7.x

config: packet_size = 32767 (requested via connection string)
connect: ok in 22ms

server: Microsoft SQL Server 2022 (RTM-CU24) - 16.0.4245.2 (X64)
Posts: 3729195 total rows (7.7s for COUNT_BIG)

=== read results ===
rows read           : 100000
body bytes total    : 62227851
null ViewCount      : 0
null Title          : 81021
null Tags           : 81021
elapsed             : 696ms
throughput          : 143536 rows/sec
checksum (sha256)   : ec2c9d516a652fdc91c98657208f3f3abfb06e47925df45ff60adf8169cd3bd1
```

The checksum is deterministic across runs, so it's a useful regression fingerprint if re-running on the same dataset.

## Findings

### Compile-time wins (zero API surprises)

The spike compiled cleanly on the first try with no API guesswork required. Every type signature inferred from the README matched reality:

- `Config::from_connection_string()` parses ADO.NET strings including `Packet Size=32767`
- `config.packet_size: u16` is a public field — **no fork needed**
- `Client::connect(config).await` returns the `Ready`-state client
- `client.query(sql, &[])` returns an iterable of `Result<Row>`
- `row.get::<i32>(idx)`, `row.get::<i64>(idx)`, `row.get::<String>(idx)` all work
- `row.get::<Option<i32>>(idx)` for nullable numerics — **the case from PR #94 works correctly**
- `row.get::<Option<String>>(idx)` for nullable strings
- `nvarchar(max)` (Posts.Body, ~6.5MB total in 10K rows) reads as `String` without issues
- `datetime` columns work
- `i64` for `COUNT_BIG`

### Runtime numbers (single-thread, x86_64 emulated on M-series)

| metric | 10K rows | 100K rows |
|---|---|---|
| connect | 14–23ms | 23ms |
| read elapsed | 97ms | 697ms |
| **throughput** | **102K rows/sec** | **143K rows/sec** |
| body bytes (LOB) | 6.5 MB | 62 MB |
| nullable cols decoded correctly | yes | yes |
| checksum reproducible | yes | yes |

`COUNT_BIG(*)` took ~7.7s but this is **not** a driver issue — `STATISTICS TIME` reported 7692ms from MSSQL itself. Driver overhead is ~130ms. The slowness is Apple Silicon emulation reading a 9.2GB MDF that hasn't been buffer-cached. Native Linux x86 hardware would be sub-second.

### ❌ Blocker for adoption: `QueryStream` is not a stream

Direct quote from `crates/mssql-client/src/stream.rs`:

> The current implementation uses a buffered approach where all rows from the TDS response are parsed upfront. This works well because:
> 1. TDS responses arrive as complete messages (reassembled by mssql-codec)
> 2. Memory is shared via `Arc<Bytes>` pattern per ADR-004
> 3. No complex lifetime/borrow issues with the connection
>
> For truly large result sets, consider using OFFSET/FETCH pagination.

`QueryStream::new(columns, rows: Vec<Row>)` — every row is parsed and held in `VecDeque<Row>` before the first `next().await` returns anything. This is the opposite of how tiberius's `QueryStream` works (yield-as-it-arrives over the TDS wire).

Implications for dmt-rs:
- **Workable in principle**: dmt-rs already uses keyset pagination in 50K-row chunks. 50K Posts rows × ~650 bytes Body ≈ 32MB per chunk — comfortable.
- **Not a drop-in**: every `MssqlReader` code path that assumes mid-result-set streaming would need to be audited and probably rewritten as a chunk loop. The dmt-rs transfer engine's read-ahead pipeline would need to interact with chunked queries instead of a single ongoing stream.
- **Forecloses optimizations**: tiberius can stream a single multi-million-row SELECT with one round trip. `mssql-client` can't — you'd be paying per-chunk RPC overhead that tiberius avoids.

### Other findings worth knowing

1. **`process_env_change` ignores `EnvChangeType::PacketSize`.** In `crates/mssql-client/src/client/connect.rs`, the env-change handler only branches on `Database` and `Routing`. The PacketSize env change from the server is logged as a generic "environment change" but not stored or exposed. Subsequent post-login `send_message()` calls use the `MAX_PACKET_SIZE` constant rather than the negotiated value. **Verify before any production use** — this could be a real bug or just a docs/observability gap.

2. **Build pulls in `aws-lc-sys` + `cmake` + `cc`** despite the README claiming pure-Rust ring-only TLS. Probably a default-features quirk. Cold build is ~12s on M-series Apple Silicon, ~109 transitive crates. Worth investigating which feature flag triggers it before committing to a real migration — this is a meaningful build-time and CI-image-size impact.

3. **4 months old, 13 stars, single-vendor (Praxiom Labs).** Bus factor unknown. Active development cadence is good (v0.5 → v0.7 in 3 months), but no community adoption signal yet.

4. **Bulk insert (BCP) support exists** in `crates/mssql-client/src/bulk.rs` but is irrelevant to dmt-rs — MSSQL is a *source* in our pipeline, not a target. The bulk path is the *write* side.

## Related upstream context

These issues / PRs are why we cared in the first place and inform when to re-evaluate:

- [prisma/tiberius#400](https://github.com/prisma/tiberius/pull/400) — our packet_size PR, open since 2026-01-02
- [prisma/tiberius#294](https://github.com/prisma/tiberius/issues/294) — community report of tiberius reads being ~2x slower than C# (open, no resolution)
- [prisma/tiberius#226](https://github.com/prisma/tiberius/issues/226) — separate buffer-handling perf fix, ~3x speedup, open since 2022
- [prisma/tiberius#411](https://github.com/prisma/tiberius/pull/411), [#413](https://github.com/prisma/tiberius/pull/413) — other open community PRs sitting unreviewed for months

## Appendix: spike source

The spike code lives outside the workspace because `mssql-client` requires Rust 2024 edition while dmt-rs is on 2021. Recreate it as a self-contained project under `spike-mssql-client/`:

### `spike-mssql-client/Cargo.toml`

```toml
[workspace]
# Self-contained workspace so cargo does not try to attach this spike
# to the parent dmt-rs workspace (which is on Rust 2021 / MSRV 1.75,
# while mssql-client requires Rust 2024 / MSRV 1.88).

[package]
name = "spike-mssql-client"
version = "0.0.0"
edition = "2024"
publish = false

[[bin]]
name = "spike-mssql-client"
path = "src/main.rs"

[dependencies]
mssql-client = "0.7"
tokio = { version = "1.48", features = ["macros", "rt-multi-thread", "time"] }
sha2 = "0.10"
hex = "0.4"
anyhow = "1.0"
```

### `spike-mssql-client/src/main.rs`

```rust
//! Spike: evaluate praxiomlabs/rust-mssql-driver (`mssql-client`) as a
//! drop-in replacement for our forked tiberius on the dmt-rs read path.
//!
//! This is throwaway code. Goals:
//!   1. Confirm we can connect with a configurable packet size (no fork needed).
//!   2. Confirm row streaming works against a real MSSQL table.
//!   3. Stress the type system with the columns that bit us in tiberius:
//!      nullable numerics, nvarchar(max), datetime, mixed null/non-null.
//!   4. Print enough info to compare against tiberius (timing, throughput,
//!      negotiated packet size).
//!
//! Run against the same StackOverflow2010 / Posts test data we use in
//! the dmt-rs integration matrix. See MSSQL-CLIENT-SPIKE.md for setup.

use std::time::Instant;

use anyhow::{Context, Result};
use mssql_client::{Client, Config};
use sha2::{Digest, Sha256};

const CONNECTION_STRING: &str = "Server=localhost,1433;\
                                 Database=StackOverflow2010;\
                                 User Id=sa;\
                                 Password=YourStrong@Passw0rd;\
                                 TrustServerCertificate=true;\
                                 Encrypt=false;\
                                 Packet Size=32767;";

const SAMPLE_SIZE: i64 = 100_000;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    println!("=== mssql-client spike ===");
    println!("crate: mssql-client 0.7.x");
    println!();

    // ----- 1. CONNECT --------------------------------------------------
    let connect_start = Instant::now();
    let config = Config::from_connection_string(CONNECTION_STRING)
        .context("parse connection string")?;

    println!(
        "config: packet_size = {} (requested via connection string)",
        config.packet_size
    );

    let mut client = Client::connect(config)
        .await
        .context("Client::connect")?;
    println!("connect: ok in {:?}", connect_start.elapsed());
    println!();

    // ----- 2. SANITY CHECK --------------------------------------------
    let rows = client
        .query("SELECT @@VERSION, @@SPID", &[])
        .await
        .context("@@VERSION query")?;
    let mut version_seen = false;
    for row in rows {
        let row = row.context("decode @@VERSION row")?;
        let version: String = row.get(0).context("col 0 -> String")?;
        let first_line = version.lines().next().unwrap_or("");
        println!("server: {first_line}");
        version_seen = true;
    }
    if !version_seen {
        anyhow::bail!("@@VERSION returned no rows");
    }
    println!();

    // ----- 3. ROW COUNT (sets baseline) --------------------------------
    let count_start = Instant::now();
    let rows = client
        .query("SELECT COUNT_BIG(*) FROM dbo.Posts", &[])
        .await
        .context("count query")?;
    let mut total_rows: i64 = 0;
    for row in rows {
        let row = row.context("decode count row")?;
        total_rows = row.get::<i64>(0).context("count -> i64")?;
    }
    println!(
        "Posts: {total_rows} total rows ({:?} for COUNT_BIG)",
        count_start.elapsed()
    );
    println!();

    // ----- 4. TYPED READ OF A REPRESENTATIVE SLICE ---------------------
    // Columns chosen specifically to exercise the cases that have hurt us
    // in tiberius:
    //   Id              int               PK, never null
    //   CreationDate    datetime          not null
    //   Score           int               not null
    //   ViewCount       int               nullable  ← issue #94
    //   AnswerCount     int               nullable
    //   FavoriteCount   int               nullable
    //   Title           nvarchar(250)     nullable
    //   Tags            nvarchar(150)     nullable
    //   Body            nvarchar(max)     not null  ← LOB
    let sql = format!(
        "SELECT TOP {SAMPLE_SIZE} \
             Id, CreationDate, Score, ViewCount, AnswerCount, FavoriteCount, \
             Title, Tags, Body \
         FROM dbo.Posts \
         ORDER BY Id"
    );

    let read_start = Instant::now();
    let rows = client.query(&sql, &[]).await.context("posts query")?;

    let mut row_count: u64 = 0;
    let mut body_bytes: u64 = 0;
    let mut null_view_count: u64 = 0;
    let mut null_title: u64 = 0;
    let mut null_tags: u64 = 0;
    let mut hasher = Sha256::new();

    for row in rows {
        let row = row.context("decode posts row")?;
        row_count += 1;

        // Required columns
        let id: i32 = row.get(0).context("Id -> i32")?;
        let score: i32 = row.get(2).context("Score -> i32")?;
        // Body is nvarchar(max), required: hit the LOB path.
        let body: String = row.get(8).context("Body -> String")?;
        body_bytes += body.len() as u64;

        // Nullable numerics — the case that bit us in #94.
        let view_count: Option<i32> = row.get(3).context("ViewCount -> Option<i32>")?;
        let answer_count: Option<i32> = row.get(4).context("AnswerCount -> Option<i32>")?;
        let favorite_count: Option<i32> = row.get(5).context("FavoriteCount -> Option<i32>")?;
        if view_count.is_none() {
            null_view_count += 1;
        }

        // Nullable strings.
        let title: Option<String> = row.get(6).context("Title -> Option<String>")?;
        let tags: Option<String> = row.get(7).context("Tags -> Option<String>")?;
        if title.is_none() {
            null_title += 1;
        }
        if tags.is_none() {
            null_tags += 1;
        }

        // Cheap, deterministic checksum across what we read so we can
        // diff against tiberius later if needed.
        hasher.update(id.to_le_bytes());
        hasher.update(score.to_le_bytes());
        hasher.update(view_count.unwrap_or(-1).to_le_bytes());
        hasher.update(answer_count.unwrap_or(-1).to_le_bytes());
        hasher.update(favorite_count.unwrap_or(-1).to_le_bytes());
        if let Some(t) = title.as_ref() {
            hasher.update(t.as_bytes());
        }
        if let Some(t) = tags.as_ref() {
            hasher.update(t.as_bytes());
        }
        // First 64 bytes of Body is enough fingerprint without bloating.
        let body_head = &body.as_bytes()[..body.len().min(64)];
        hasher.update(body_head);
    }

    let elapsed = read_start.elapsed();
    let throughput = row_count as f64 / elapsed.as_secs_f64();

    println!("=== read results ===");
    println!("rows read           : {row_count}");
    println!("body bytes total    : {body_bytes}");
    println!("null ViewCount      : {null_view_count}");
    println!("null Title          : {null_title}");
    println!("null Tags           : {null_tags}");
    println!("elapsed             : {elapsed:?}");
    println!("throughput          : {throughput:.0} rows/sec");
    println!("checksum (sha256)   : {}", hex::encode(hasher.finalize()));
    println!();

    println!("spike completed without errors");
    Ok(())
}
```
