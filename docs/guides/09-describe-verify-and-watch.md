# 9. Describe, verify and watch your data

Numbers are only useful when everyone reads them the same way. Each
dataset's **About** view holds three things that make answers dependable,
for people and for AI assistants alike. None of them needs an AI model or
costs anything.

Who can change them: an administrator, or the person who added the dataset.
Everyone who can see the dataset can read them.

## Describe what the data means

1. Open the dataset and select **About**.
2. Under **What this data means**, say what one row is, where the data comes
   from, and how it should and should not be added up. For example:
   *"One row per sale, recorded once by the branch that made it. Region and
   company totals are sums of these rows; never add their own reports on
   top."*
3. Give any column a short meaning, such as *"In euros, before tax."*
4. Select **Save descriptions**.

Descriptions also appear in the API and are read by AI assistants connected
over MCP before they answer.

## Verify the questions that matter

A verified question is a chart someone responsible for the data has
checked, saved with the question it answers.

1. On the **Chart** view, draw the answer: for example *Sum of amount by
   region*, with any filter you need.
2. Open **Save as a verified question**, write the question in plain words,
   and select **Save**.

The answer is worked out again from the current data each time, the same
way, and shows who verified it and when. AI assistants are told to use a
verified question when one matches, and to say who verified it.

If the data changes so that a question no longer fits (for example a column
is removed), the answer says so instead of guessing.

## Watch for problems

A watch is a check that runs every hour and at start-up. Add one under
**Add a watch** on the About view. There are three kinds:

| Check that | Example | Alerts when |
| --- | --- | --- |
| **No record appears twice** | Key columns `order_id`: the same sale reported by a branch and again by its region | more combinations repeat than the number you give (usually 0) |
| **A total stays within a limit** | Sum of `amount` above 10,000 | the total goes above, or below, the number |
| **The latest period changes little** | Number of rows by month | the latest period differs from the one before by at least that percentage |

The **Watches** page lists every watch on datasets you can see, alerts
first. Select **Check now** on the About view to run one straight away.

A watch that cannot run on the data as it is now is refused when you add
it. If the data later changes so that it cannot run, it shows **Cannot
check** with the reason.

The latest period of a change watch may not be complete yet (the current
month, for instance), so its message says so.
