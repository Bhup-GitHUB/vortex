# vortex-sqllogictest

This crate tests correctness of our DuckDB and DataFusion integrations. It runs
[.slt](https://github.com/risinglightdb/sqllogictest-rs) tests with our queries
as well as 0.1 scale of TPC-H, TPC-DS, and 1 shard of partitioned ClickBench.
For last three query sets, tests also verify each query's `EXPLAIN` result.

## Running tests

```sh
# For running our own queries
cargo test -p vortex-sqllogictest --test sqllogictests

# For running TPC-H, TPC-DS, or ClickBench you need to generate data first
./slt/generate_data.sh
# You can generate data for a specific dataset
./slt/generate_data.sh tpcds
# Then you can run the tests
cargo test -p vortex-sqllogictest --test sqllogictests

# You can run only DuckDB tests...
cargo nextest run -p vortex-sqllogictest -E 'test(/slt::duckdb::/)'
# ... or a single test on both DuckDB and DataFusion
cargo nextest run -p vortex-sqllogictest -E 'test(strings)'

# You can also list all tests
cargo nextest list -p vortex-sqllogictest
```

## Writing tests

To run a test only for DuckDB or DataFusion, place test in `slt/duckdb` or
`slt/datafusion`. To run a query inside test only for DuckDB or DataFusion,
place `onlyif duckdb` or `onlyif datafusion` before query.

To match output by regex in DuckDB, use `<REGEX>:pattern` or `<!REGEX>:pattern`
in query result.

To regenerate .slt file to match current output, use `--complete`.

## Encoding checks

`slt/<suite>/filecheck/<table>.check` checks the array encodings of the generated
`slt/<suite>/data/<table>.vortex` with `vx filecheck`, so a compressor change that
re-encodes benchmark data fails here instead of only showing up as a benchmark swing.
The directives match the file's encoding-tree dump partially, like LLVM FileCheck, and
`units: <glob>` applies the directives that follow it to every matching column chunk.
See the `vortex_tui::filecheck` module docs for the format.

```sh
# Print the dump a check file is matched against
cargo run -p vortex-tui --bin vx -- filecheck slt/tpch/data/lineitem.vortex
# Reproduce a failing check with full match explanations
cargo run -p vortex-tui --bin vx -- filecheck slt/tpch/data/lineitem.vortex \
  slt/tpch/filecheck/lineitem.check --explain
# Run only the encoding checks
cargo nextest run -p vortex-sqllogictest -E 'test(/slt::filecheck::/)'
```

Check files are written by hand; `--complete` does not rewrite them.

```sh
cargo test -p vortex-sqllogictest --test sqllogictests -- --complete
# Regenerate output for a specific test
cargo test -p vortex-sqllogictest --test sqllogictests -- --complete strings
```
