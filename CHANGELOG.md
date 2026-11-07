# Changelog

## 1.1.1 (2026-11-05)

Fixes from a code review.

- A stack line is sent only if it reads as a Rust function path. Free text, such as a name or a sentence, is dropped.
- Only programs named `trindle` or starting `trindle-` are reported; a note from any other program is ignored.
- The notes file is kept until the reports from it are saved, so a full disk or a stopped run no longer loses reports.
- Two runs at once (the timer and a manual send) no longer happen: the second stops at once.
- `packaging/build-deb.sh` no longer forces `--offline`, so it works on a machine with an empty Cargo cache. It uses `--locked`.
- The README says "by design" instead of "by construction".

## 1.1.0 (2026-10-28)

Brought in line with Buildmorphic's report API. Version 1.0.0 would have been refused on every send.

- Reports go to `https://buildmorphic.com/api/reports/<repository>`, after asking whether it is taking reports. If not, they wait.
- A report goes to the repository that owns the crashed program: Trindle's other programs each have their own, and everything else goes to `trindle`.
- Each report now has the fields Buildmorphic takes: the kind, the repository, the version and a `crash` holding the program, the signal and the stack as plain text. An error is sent as a crash with its code as the signal.
- The computer description is sent as text, with a one-line summary first.
- A Buildmorphic username or GitHub name saved in Trindle Settings is sent with each report, if it fits Buildmorphic's rules. The key and the linking step are gone: Buildmorphic has neither.
- A run stops for the hour on any server fault (5xx), not only on a refusal for too many.
- At most 1 MB of notes is read in a run, and at most 200 reports wait.
- Repeats are no longer counted here. Buildmorphic counts the same fault itself; the add-on still sends a fault once an hour at most.
- A refused report (400, 403 or 413) is dropped. Too many reports from this computer, or no connection, keeps them waiting for the next hour.
- Reports waiting from 1.0.0 are dropped on upgrade.

## 1.0.0 (2026-10-01)

First release.

- Sends the crash and error notes that Trindle's programs leave, once an hour, as anonymous reports.
- The same fault is sent once, with a count.
- Crashes, errors and the rough computer description each have a switch in Trindle Settings.
- Reports that cannot be sent are tried again each hour for up to a day.
- The last 50 reports sent are kept on your computer so Settings can show them.
- Comes as a Debian package that starts an hourly timer.
