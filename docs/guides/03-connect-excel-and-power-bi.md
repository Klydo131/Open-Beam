# 3. Connect Excel and Power BI

Open Beam publishes your datasets as an OData feed, a standard format that
Excel and Power BI can read without installing a driver.

> Menu names below follow Microsoft's documentation for Excel for Microsoft 365
> and Power BI Desktop and can differ between versions. If a step does not
> match what you see, please [open an issue](https://github.com/Klydo131/Open-Beam/issues).

## Before you start

1. Start Open Beam: `cargo run --release -- --config beam.toml`.
2. Note the feed address it prints, for example `http://127.0.0.1:8787/odata/`.
3. Keep the window open. The feed is available only while Open Beam is running.

## Excel

1. On the **Data** tab choose **Get Data → From Other Sources → From OData Feed**.
2. Paste the feed address and choose **OK**.
3. When asked how to sign in, choose **Anonymous** for a local feed, or
   **Basic** and enter the username and password if the feed is protected.
4. Pick a dataset in the Navigator, then choose **Load**.

Use **Data → Refresh All** to fetch the latest data.

## Power BI Desktop

1. Choose **Get Data → OData feed**.
2. Paste the feed address and choose **OK**.
3. Choose **Anonymous**, or **Basic** with the username and password.
4. Pick a dataset in the Navigator, then choose **Load** or **Transform Data**.

Filters, column selection and sorting applied in Power Query are sent to Open
Beam where possible, so only the rows you need are transferred.

If your feed asks for a password, see
[Protect a feed with a password](05-protect-a-feed.md).

## Reference

| Address | Returns |
| --- | --- |
| `/odata/` | List of datasets |
| `/odata/$metadata` | Column names and types |
| `/odata/<dataset>` | Rows |
| `/odata/<dataset>/$count` | Number of rows |
| `/healthz` | `ok` when the server is running |

Supported query options: `$select`, `$filter`, `$orderby`, `$top`, `$skip`,
`$count`.

Supported in `$filter`: `eq ne gt ge lt le`, `and or not`, parentheses, and the
functions `contains`, `startswith`, `endswith`, `tolower`, `toupper`, `trim`,
`length`, `year`, `month`, `day`.

Timestamps without a time zone are served as UTC.

Next: [Combine and clean data](04-combine-and-clean-data.md).
