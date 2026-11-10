# 7. Add, share and export data

## Add a file

1. Select **Add data**.
2. Choose a file:
   - **CSV** (also `.tsv` and `.txt`)
   - **Excel workbook** (`.xlsx`). The first sheet is used, and dates stay dates.
   - **Parquet**
3. Select **Add data**. The table opens straight away.

The first row should hold the column names. Files are limited to 50 MB by
default; an administrator can change this with `max_upload_mb` in
`beam.toml`.

Open Beam stores its own copy of the file. Changing the original afterwards
does not change the dataset; add the new version as a new dataset.

![The Add data page with a file chosen](../images/screens/add-data.png)

## Who can see it

A dataset you add is visible to you and to administrators only, until you
share it.

1. Open the dataset and select the **Sharing** tab.
2. Choose a colleague and select **Share**. People in your contacts are listed
   first.
3. To undo, select **Stop sharing** beside their name.

People you share with can view, chart and download the dataset, and load it
into Excel or Power BI. They cannot share it onwards or delete it.

To delete a dataset, use the **Sharing** tab, tick the confirmation and
select **Delete dataset**.

## Contacts

The **People** page lists everyone with an account. Select **Add contact**
beside the colleagues you work with most. Contacts are personal: nobody is told
when you add them.

![The People page with one contact](../images/screens/people.png)

## Take data somewhere else

Every table has a **Use this data somewhere else** panel.

| To use it in | Do this |
| --- | --- |
| Excel | Select **Download Excel** and open the file. |
| Google Sheets | Select **Download CSV**, then in Sheets choose **File → Import → Upload**. |
| Power BI or Excel, kept up to date | Choose **Get Data → OData feed**, enter the address shown, and sign in with **Basic** using your Open Beam username and password. Refreshing fetches the latest data. |

Downloads contain exactly what is on screen: the same filter, sort and columns,
up to 100,000 rows. For more rows, use the Power BI or Excel connection.

Google Sheets cannot connect live yet: Sheets fetches data from Google's
servers, which cannot sign in to Open Beam on your computer.

In **Advanced** mode the panel also gives the address and a Power Query
formula for exactly the view on screen.
