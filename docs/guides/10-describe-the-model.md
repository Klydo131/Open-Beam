# Describe the model

Datasets hold rows; people think in customers, orders and branches. A
**model** (an ontology) says which business object each dataset holds,
which column tells them apart, the other words people use for them, and
how they link. People see it on the **Model** page; AI assistants read it
with the `describe_model` tool, so a question about "clients" finds the
customers, and orders are joined to customers by the right column.

## Write it

An administrator writes the model in a file, for example `ontology.toml`
beside `beam.toml`, and names it there:

```toml
ontology = "ontology.toml"
```

Start from `ontology.example.toml`. It has two parts.

**Objects.** One per dataset: the dataset, the column whose value each row
has on its own (the key), and optionally a label, other words for it and a
description.

```toml
[[object]]
name = "Customer"
dataset = "customers"
key = "customer_id"
synonyms = ["client", "account"]
description = "A person or company that buys from us."
```

**Links.** Each row of one object names one of another, in a column:

```toml
[[link]]
name = "placed_by"
from = "Order"
to = "Customer"
column = "customer_id"
```

## Joined datasets

Each link adds a dataset that joins the two, named after them:
`orders_with_customers` holds every order with its customer's columns,
named after the object (`customer_name`, `customer_team`). It works
everywhere a dataset does: the table and chart, the profile, Excel and
Power BI, the API, AI assistants and watches. Everyone who can see the
two datasets can see it.

- Every order appears once, in its own order. If a customer key repeats
  (the Model page shows it), one of those rows is used, the same one
  every time. An order whose customer is missing keeps empty values.
- When both datasets are kept as fast copies, so is the join, so it
  reads as quickly as either of them. It is made again when either file
  changes and the server restarts.
- If a customer key repeats, the first of its rows in the data is used.
- A looked-up column that would have the same name as one of the
  dataset's own (an order that already has `customer_name`) is refused,
  rather than renamed where nobody would notice.
- The join is made when the server starts, before it accepts visitors;
  very large datasets make the start slower the first time.
- A joined name may not be the name of anyone's upload: descriptions,
  verified questions and watches are kept by name, so the new dataset,
  which everyone sees, would show what the upload's owner wrote about
  their private data. The server refuses to start and names the upload.
- Two links between the same pair of objects need their own names; give
  one with `joined`:

```toml
[[link]]
name = "shipped_to"
from = "Order"
to = "Customer"
column = "ship_to_id"
joined = "orders_with_ship_to"
```

## Metrics

A metric is a business figure defined once, so everyone, and every AI
assistant, gets the same number for "revenue":

```toml
[[metric]]
name = "revenue"
object = "Order"
aggregate = "sum"        # count, sum, avg, min or max
column = "amount"        # a number column; not used by count
label = "Revenue"
description = "All orders, before refunds."
```

A metric is one of five aggregates over one number column of an object.
Nothing else can be written, so a metric is always plain to read and can
never become arbitrary code. The Model page shows each metric's
definition and current value. Assistants use the `get_metric` tool, which
works it out by its definition, in total or split by a column or period,
and filtered.

## What is checked

When the server starts, the model must be exact; otherwise Open Beam
refuses to start and lists every problem:

- every dataset, column and object it names exists (nothing is guessed);
- it names only datasets from `beam.toml`: uploads are private, and a
  model shown to everyone must not reveal them;
- each dataset describes one object, and each word (name, label or
  synonym) means one object only, so nobody, person or assistant, has to
  guess which is meant;
- a link joins like values: numbers to numbers, text to text, and never
  an object to itself;
- each joined dataset has a name of its own, not already a dataset's;
- descriptions are plain text of reasonable length.

The **Model** page then checks the model against the data, and again
whenever datasets change:

- **Each of N rows has its own key**, or how many keys repeat or are
  empty. A key that repeats cannot tell objects apart.
- **All N point to one**, or how many rows of a link point to nothing:
  an order naming a customer who is not in the customers dataset.

## Where it comes from

The design takes ideas, not code, from open projects: object types with a
key and checked links in the style of typed graph schemas, a closed-world
check that rejects any undeclared name, and a short reason on every link.
It uses the W3C SKOS idea of preferred and alternative labels for
synonyms. Validity rules that become watches are planned next.
