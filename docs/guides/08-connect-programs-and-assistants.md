# 8. Connect programs and AI assistants

Excel and Power BI connect through the data feed ([guide 3](03-connect-excel-and-power-bi.md)).
This guide covers everything else that can read your data: your own scripts,
SQL tools, and AI assistants. All of it is **read-only**: nothing connected
this way can change or delete data, and each connection sees exactly the
datasets its account can see in the browser.

## Step 1: Create a personal API token

Programs sign in with a token instead of your password, so you can give each
program its own and take it away without changing your password.

1. Sign in and open **Settings**.
2. Under **API tokens**, type a name that says where it will be used, such as
   `reports script` or `assistant on my laptop`, and select **Create token**.
3. Copy the token now. It starts with `obt_` and is shown only once; Open
   Beam keeps only a fingerprint of it.

To stop a program, select **Revoke** next to its token. Each account can hold
up to 20 tokens, and a disabled account's tokens stop working at once.

Treat a token like a password: anyone who has it can read what you can read.

## Step 2: Choose how to connect

| You want to | Use |
| --- | --- |
| Fetch data from a script or another app | The REST API |
| Save a filtered view as a Parquet file | The REST API's Parquet download |
| Let an AI assistant look things up | The MCP endpoint |
| Use psql, DBeaver, Python, R or another SQL tool | PostgreSQL access |

### The REST API

Send the token in the `Authorization` header:

```bash
curl -H "Authorization: Bearer obt_your_token" http://127.0.0.1:8787/api/v1/datasets
```

| Address | Returns |
| --- | --- |
| `/api/v1/datasets` | The datasets you can see, with their columns |
| `/api/v1/datasets/{name}` | One dataset's columns and key |
| `/api/v1/datasets/{name}/rows` | Rows, with `filter`, `select`, `orderby`, `top`, `skip` and `count` |
| `/api/v1/datasets/{name}/rows.parquet` | The same rows as a Parquet file, up to 5,000,000 |
| `/api/v1/datasets/{name}/summary` | Totals grouped by a column: `group`, `aggregate` (count, sum, avg, min, max), `measure`, `bucket` (day, month, quarter, year), `filter` |
| `/api/v1/openapi.json` | A machine-readable description of all of the above (OpenAPI 3.1) |

Filters use the same words as the data feed, for example
`filter=region eq 'EU' and amount gt 100`. An option Open Beam does not
recognise is refused rather than silently ignored, so a typing mistake can
never return the wrong rows.

### AI assistants (MCP)

Open Beam includes a read-only [Model Context Protocol](https://modelcontextprotocol.io)
server at `/mcp`. An assistant connected to it can list datasets, describe
their columns, read up to 200 rows at a time, and calculate totals. Totals
are calculated by Open Beam, not guessed by the assistant, so they match
what you see in the browser.

Any MCP client that supports the Streamable HTTP transport and custom
headers can connect. For example, with Claude Code:

```bash
claude mcp add --transport http open-beam http://127.0.0.1:8787/mcp --header "Authorization: Bearer obt_your_token"
```

Open Beam tells the assistant to treat everything inside your datasets as
data, never as instructions. Even so, only connect an assistant you trust to
datasets you are willing to show it.

### PostgreSQL access

Many tools speak PostgreSQL's protocol. Open Beam can answer them with plain
SQL `SELECT` statements. It is **off** until you turn it on in `beam.toml`:

```toml
postgres_bind = "127.0.0.1:5433"
```

Restart Open Beam; it prints the address. Then connect with your Open Beam
username and a token as the password:

```bash
psql "host=127.0.0.1 port=5433 user=yourname dbname=beam sslmode=disable"
```

```sql
SELECT region, count(*) AS orders, sum(amount) AS total
FROM sales
GROUP BY region
ORDER BY total DESC;
```

What to expect:

- **Only `SELECT`.** Anything that would change data, settings or files is
  refused. Queries can join, filter, group, sort and use common functions
  for text, numbers and dates.
- **Only your datasets.** A dataset you cannot see behaves exactly like one
  that does not exist. `information_schema.tables` and
  `information_schema.columns` list just your datasets.
- **Tokens only.** Your account password is not accepted here, because this
  protocol sends the password unencrypted. Keep `postgres_bind` on
  `127.0.0.1`, or reach it across a network only through an encrypted
  tunnel such as SSH.
- **Tool menus may be limited.** Tools that browse a database by reading
  PostgreSQL's internal catalog (for example psql's `\d` commands) will not
  find what they look for; type a `SELECT` instead. Excel and Power BI
  should keep using the [data feed](03-connect-excel-and-power-bi.md): their
  PostgreSQL connectors read that internal catalog when they connect, and
  have not been tested with Open Beam.

## Time limits

Every query, from any of these connections and from the browser, stops after
30 seconds and says so. An administrator can change the limit in
`beam.toml`, from 1 to 3600 seconds:

```toml
query_timeout_seconds = 60
```
