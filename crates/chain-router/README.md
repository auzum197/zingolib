# chain-router

Route Zcash chain requests across many data sources, by the trust level that
you give each data source.

With `chain-router` you can:

- Give each data source a trust level.
- Write wallet code one time, against one typed data model, for all data
  sources.
- Continue to read the chain when a data source fails.
- Require a quorum of untrusted data sources before you accept a result.
- Accept or reject each result by its data source.
- Measure the duration and the result of each request to each data source.
- Send a request that exposes wallet data only to the data sources that you
  select.
- Replace the routing policy, add a type of data source, or add a transport,
  from your own crate.
