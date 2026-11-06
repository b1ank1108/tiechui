# Loop

Enabled by default; `[modules] loop = false` removes its commands, tools and
scheduler.

`/loop [interval] <prompt>` schedules a session-only recurring prompt, runs it
once as soon as the session is idle and then on the schedule. The interval is
the first token (`30s`, `5m`, `2h`, `1d`) or a trailing `every 20m` /
`every 20 minutes`; without one it is 10 minutes. Seconds round up to a minute.
Other periods round to the nearest even cron period, ties to the longer one
(`7m` runs every 6 minutes, `90m` every 2 hours); the confirmation says so.
`1d` is daily at local midnight and `Nd` steps the day of the month, so the gap
at month end can be shorter. `/cron-list` and `/cron-delete <id>` work while a
turn runs; deleting does not stop a run in progress.

The tools are `cron_create({cron, prompt, recurring?, durable?})`, `cron_list({})`
and `cron_delete({id})`. Crons have five fields in local time (`*`, numbers,
`*/N`, `N-M`, `N-M/S`, lists; weekday 7 is Sunday; day of month and weekday
match either when both are restricted) and must match within a year. Creation
does not run the prompt. A scheduled run cannot create tasks. Prompts starting
with a `/command` are rejected; `$skill` references load on every run.

Due tasks wait until the actor is idle, like goal continuations, and run as a
visible user message through the normal turn, tools and permissions. Missed
periods run once. Recurring runs are delayed by up to 10% of the period (at most
15 minutes), and one-shots on :00 or :30 run up to 90 seconds early, both stable
per task ID. A recurring task created 7 days ago runs one last time and is
deleted; a one-shot is deleted when it runs. At most 50 tasks exist across the
session and the project.

Session-only tasks belong to the session: they survive `/reload` and end with
`/new`, a resume of another session, a fork, or exit. Esc stops a run, not the
schedule.

`durable: true` saves the task to `.agents/scheduled_tasks.json` at the Git root
(or the working directory) of a trusted project; the file makes an otherwise
plain project require trust. Changes are read, checked and replaced under
`scheduled_tasks.lock`; an unreadable or newer file pauses durable tasks and is
never overwritten. One interactive session per project holds
`scheduled_tasks.owner.lock` and runs durable tasks; others take over within
5 seconds of it ending. Runs are recorded within a second, so a crash right
after a run can repeat a recurring task once. One-shots that came due while no
session ran them are not run: the next owner asks the model to check with the
user first, and removes them once that turn starts. `tc exec` manages durable
tasks but runs none; session-only creation there is an error.
