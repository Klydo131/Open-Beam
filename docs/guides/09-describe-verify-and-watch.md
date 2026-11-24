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

## Check the figures: the profile

The About view also shows a **Profile** of every number column (the first
12). It is worked out the first time the page opens and kept until a
dataset is added or removed, or the server restarts (which is also when
changed data files are read again), so it always matches the data shown:

- **Rows, values and total.** These are control totals, the check
  bookkeepers have used for centuries: compare them with the source
  (the total of the column in the original spreadsheet or system) to
  confirm nothing was lost or counted twice on the way in.
- **Smallest, lower quarter, middle, upper quarter, largest.** Where the
  values lie: half of them are between the two quarter marks.
- **Far out.** How many values lie more than one and a half times the
  distance between the quarter marks beyond them (Tukey's fences). They
  are worth a look, not necessarily wrong: a large order is real, a
  misplaced decimal point is not.

## Watch for problems

A watch is a check that runs every hour and at start-up. Add one under
**Add a watch** on the About view. There are four kinds:

| Check that | Example | Alerts when |
| --- | --- | --- |
| **No record appears twice** | Key columns `order_id`: the same sale reported by a branch and again by its region | more combinations repeat than the number you give (usually 0) |
| **A total stays within a limit** | Sum of `amount` above 10,000 | the total goes above, or below, the number |
| **The latest period changes little** | Number of rows by month | the latest period differs from the one before by at least that percentage |
| **The latest period stays in its usual range** | Sum of `amount` by month | the latest period is outside the range the figure usually moves in, or the last 8 periods all sit above (or all below) its usual middle |

The **Watches** page lists every watch on datasets you can see, alerts
first. Select **Check now** on the About view to run one straight away.

A watch that cannot run on the data as it is now is refused when you add
it. If the data later changes so that it cannot run, it shows **Cannot
check** with the reason.

The latest period of a change watch may not be complete yet (the current
month, for instance), so its message says so.

### Usual ranges: let the data say what is normal

Picking a limit by hand is hard: set it too tight and the watch cries
wolf over ordinary ups and downs; too loose and it misses a real change.
A usual-range watch needs no number. It looks at the figure's own recent
history, up to 20 periods, and works out the range it normally moves
in. It needs at least 8 periods, and looks for shifts once there are 16.

It alerts in two cases:

- **Out of the ordinary.** The latest period is outside the usual range:
  something happened that the normal ups and downs do not explain.
- **A shift.** The last 8 periods were all above (or all below) the usual
  middle. No single period stands out, but the figure has moved, and
  ordinary ups and downs rarely line up eight times in a row.

This is the process behaviour chart (the "XmR" chart) that quality
engineers have used since the 1920s, with the usual 3-sigma limits worked
out from medians so that one odd period in the history does not distort
them. Like any statistical check it assumes the periods are roughly
comparable: a figure with a strong season (every December is high) will
alert on the season; watch it by year, or compare it with the same period
last year instead.
