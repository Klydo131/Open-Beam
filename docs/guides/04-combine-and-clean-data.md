# 4. Combine and clean data

Raw files are rarely what you want to present. A *query dataset* is a dataset
defined by a SQL `SELECT` over datasets you have already declared. Excel and
Power BI see it as an ordinary table, always computed from the current files.

Use query datasets to summarise, join, and remove duplicates in one place, so
every report built on top agrees.

## Step 1: Summarise

The example configuration already contains this dataset, below `sales`:

```toml
[[dataset]]
name = "sales_by_region"
key = "region"
sql = """
SELECT region,
       sum(units)              AS units,
       sum(units * unit_price) AS revenue
FROM sales
GROUP BY region
"""
```

Select **sales_by_region** on the Datasets page to see the result.

## Step 2: Join two files

Declare both files, then a query dataset that joins them:

```toml
[[dataset]]
name = "regions"
path = "data/regions.csv"   # columns: region, manager
key = "region"

[[dataset]]
name = "sales_with_manager"
sql = "SELECT s.*, r.manager FROM sales AS s JOIN regions AS r USING (region)"
```

## Step 3: Count each thing once

When several people report the same event, adding up their totals counts it
more than once. Totals cannot be corrected after the fact, because a total
does not say *which* items it contains.

The reliable fix is to collect one row per item with an identifier, and let
Open Beam work out the totals:

```toml
[[dataset]]
name = "submissions"
path = "data/submissions.csv"   # columns: item_id, submitted_by, submitted_on
key = "submission_id"

# One row per item, however many times it was submitted.
[[dataset]]
name = "items"
key = "item_id"
sql = """
SELECT item_id,
       min(submitted_on)                    AS first_submitted,
       count(*)                             AS times_submitted,
       string_agg(DISTINCT submitted_by, '; ') AS submitted_by
FROM submissions
GROUP BY item_id
"""

# The audit trail: what was submitted more than once, and by whom.
[[dataset]]
name = "duplicate_submissions"
key = "item_id"
sql = "SELECT * FROM items WHERE times_submitted > 1"
```

Present `items` for the figures, and keep `duplicate_submissions` so anyone
can see exactly what was merged and why.

### Worked example

`examples/reporting` applies this to figures that travel up an organisation:
branches report to regions, and regions to head office. Two teams at one
branch each report the same ten sign-ups.

```bash
cargo run --release -- --config examples/reporting/reporting.toml
```

Open the `quarterly_summary` dataset. For sign-ups it shows:

| Branch | Reported | Confirmed | Duplicates removed | Needs review |
| --- | --- | --- | --- | --- |
| Harbour Branch | 20 | 10 | 10 | 0 |
| Hillside Branch | 8 | 6 | 2 | 2 |
| Market Branch | 4 | 4 | 0 | 0 |

At Hillside two sign-ups were reported twice with different dates. They are
counted once and flagged, because deciding which date is right needs a person.
`duplicate_reports` lists exactly which reports were merged.

## Rules

- A query dataset may use only datasets declared **above** it.
- It must be a single `SELECT` statement. Anything else is refused at start-up.
- It cannot read files that are not themselves declared as datasets.
- SQL is the [DuckDB dialect](https://duckdb.org/docs/sql/introduction).

Open Beam checks every query dataset when it starts and reports the first
problem, so a mistake never reaches the people using your feed.

Next: [Manage accounts](05-manage-accounts.md).
