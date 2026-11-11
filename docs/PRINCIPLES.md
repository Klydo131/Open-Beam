# Principles

These guide every feature in Open Beam. When a change conflicts with one of
them, the principle wins or the change is discussed first.

## Combining data must make results more accurate, never less

Open Beam exists because combined reports are often wrong in a specific way:
the same thing is counted more than once as figures pass from team to team.
If ten people joined and two teams each report the same ten, a total of
twenty is wrong, and no arithmetic on totals can fix it. Every feature that
combines data follows these rules.

1. **Count things, not totals.** Combine individual records (one row per
   person or event), never totals that may overlap.
2. **Identify before combining.** De-duplication needs a key that says two
   rows are the same thing, such as a member number or an order id. Without
   one, Open Beam says it cannot tell, rather than guessing.
3. **Show reported versus confirmed.** A combined result shows how many were
   reported, how many remain after de-duplication, and how many were removed.
4. **Never silently merge conflicts.** When two records of the same thing
   disagree, they are flagged for a person to decide.
5. **Keep the trail.** Every removed or merged record can be traced to its
   source rows. Source files are never edited; clean-up is applied when data
   is read.
6. **Explain every number.** A figure can always be followed back to the rows
   it came from.
7. **Simple by default, detail on request.** Basic mode shows the accurate
   answer and any warning; Advanced mode shows the rows, keys and rules.

## Secure and private by default

Data stays where its owner keeps it. Nothing is shared until its owner shares
it. New data is reviewed before it becomes visible to anyone else.

## Free to run

Open Beam runs on a person's own computer or server with no paid services.

## Proven, not asserted

A behaviour is not done until a test shows it, and a security rule is not
trusted until breaking it on purpose makes a test fail. See
[CONTRIBUTING.md](../CONTRIBUTING.md).
