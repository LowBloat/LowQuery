# Patched oracledb

This is `oracledb` 26.0.0-beta.4 from crates.io (Oracle's Rust thin driver,
UPL-1.0 OR Apache-2.0) with four fixes; everything else is unchanged.

1. `src/messages/execute.rs`: the execute response now knows the number of
   columns of a statement that was already described. When a query runs again
   on the same connection, the server sends no describe information, and the
   size of each duplicate-column bit vector was taken from the number of
   columns actually sent instead. With more than 8 columns and repeated values,
   the rest of the reply was misread ("internal error: unknown TTC message
   type ...") and the connection was left unusable.
2. `src/response/mod.rs`: a bit vector is cleared after the row it belongs to,
   as python-oracledb does, so a later row without one is not read with stale
   duplicate flags.
3. `src/client/mod.rs`, `src/statement/cache.rs`: a query with LOB columns
   is executed, then sent again to define those columns (as text, or as LOB
   locators). When either step failed, for example with an error raised while
   fetching the first rows, the statement went back to the statement cache
   still marked as needing a define. Its next execution then only sent the
   define and fetched from a cursor that was never executed, failing with
   ORA-01002 (fetch out of sequence), and so did every later execution of the
   same SQL in the session. The cursor of such a statement is now closed and
   the next execution parses it again, as python-oracledb does after an
   error.
4. `src/metadata.rs`, `src/statement/mod.rs`: when a query that ran before is
   parsed again by the server (statistics feedback, new statistics, other
   invalidations), the server describes its columns again on the next
   execution, LOB columns as CLOB/BLOB, although the cursor keeps its define
   and returns them as LONG/LONG RAW along with the rows. The driver took that
   description as a new LOB column and sent a define after the rows had been
   fetched, failing with ORA-01002 (fetch out of sequence). On Oracle 23ai
   this showed on about the third run of a cached `DBMS_METADATA` query. Like
   python-oracledb's `_adjust_metadata`, a column described as CLOB/BLOB
   whose previous type on the same cursor was a string/LONG (or RAW/LONG
   RAW) keeps the LONG type and needs no define.

`drivers::integration_tests::oracle_wide_rows_with_repeated_values` covers
the first two and `oracle_clob_queries_run_repeatedly` the last two, on
Oracle 21c and 23ai. Remove the `[patch.crates-io]` entry in `Cargo.toml` and this
folder once an upstream release includes the fixes.
