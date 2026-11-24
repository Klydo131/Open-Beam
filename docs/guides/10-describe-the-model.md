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

## What is checked

When the server starts, the model must be exact; otherwise Open Beam
refuses to start and lists every problem:

- every dataset, column and object it names exists (nothing is guessed);
- it names only datasets from `beam.toml`: uploads are private, and a
  model shown to everyone must not reveal them;
- each dataset describes one object, and each word (name, label or
  synonym) means one object only, so nobody, person or assistant, has to
  guess which is meant;
- a link joins like values: numbers to numbers, text to text;
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
synonyms. Metrics and joins along links are planned next.
