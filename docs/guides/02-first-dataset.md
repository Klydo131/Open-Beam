# 2. Share your first dataset

A *dataset* is a table Open Beam makes available to other tools. This guide
turns a CSV file into one.

## Step 1: Put a file in place

Create a folder named `data` inside the Open Beam folder and save a CSV file
in it as `sales.csv`. Any CSV with a header row works. To follow along
exactly, use this:

```csv
order_id,order_date,region,product,units,unit_price
1001,2025-01-06,Europe,Espresso Beans 1kg,12,18.50
1002,2025-01-06,North America,Filter Papers 100pk,40,4.25
1003,2025-01-09,Asia Pacific,Pour-over Kettle,6,42.00
1004,2025-01-14,Europe,Burr Grinder,3,129.00
```

The `data` folder is ignored by Git, so your files are never committed by
accident.

## Step 2: Create your configuration

Copy the example configuration:

```bash
cp beam.example.toml beam.toml
```

On Windows Command Prompt use `copy beam.example.toml beam.toml`.

The part that matters is the dataset entry:

```toml
[[dataset]]
name = "sales"
path = "data/sales.csv"
key = "order_id"
```

- `name` is what Excel and Power BI will show. Use letters, digits and
  underscores.
- `path` is the file, relative to `beam.toml`. CSV and Parquet are supported.
- `key` is a column whose values are unique and never empty. Leave it out if
  you have none and Open Beam will number the rows for you in a `_row` column.

Add another `[[dataset]]` block for each additional file.

## Step 3: Start Open Beam

```bash
cargo run --release -- --config beam.toml
```

You should see:

```text
Open Beam 0.1.0 serving 1 dataset(s)
  OData feed: http://127.0.0.1:8787/odata/
  Sign-in:    none (local only)
```

Leave this window open. Press `Ctrl+C` to stop.

## Step 4: Look at your data

Open <http://127.0.0.1:8787/odata/sales> in a web browser. You will see your
rows as JSON. Column names are tidied so every tool can use them: spaces and
symbols become underscores.

Try a filter by opening:

```text
http://127.0.0.1:8787/odata/sales?$filter=region eq 'Europe'&$select=product,units
```

## If something goes wrong

| Message | What to do |
| --- | --- |
| `cannot read beam.toml` | Run the command from the Open Beam folder, or pass the full path with `--config`. |
| `dataset 'sales': cannot read ...` | Check the `path` and that the file exists. |
| `key column '...' does not exist` | The `key` must match a column header exactly. |
| `cannot listen on 127.0.0.1:8787` | Another program is using the port. Change `bind` to another port such as `127.0.0.1:8788`. |

Next: [Connect Excel and Power BI](03-connect-excel-and-power-bi.md).
