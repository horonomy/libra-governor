# HORO-1154 — Tracking sheet

The actual usable sheet is `tracking-sheet-template.csv` in this same
directory — import it into a spreadsheet tool (Google Sheets, Excel,
Numbers) for real tracking; CSV was chosen over a Markdown table because
this needs to be filtered/sorted by a real person tracking up to 20 live
rows, which a Markdown table handles poorly.

No real participant data is checked into this template — every cell is a
`<placeholder>` marker. Do not commit real names, emails, or other PII
into this repository; keep the filled-in sheet in whatever private
tool (a private Google Sheet, a local spreadsheet file, etc.) the
founder normally uses for this kind of tracking data, outside the repo.

## Column reference

| Column | Meaning |
|---|---|
| `participant_id` | An internal short id (e.g. `p01`), not a real name |
| `contact_placeholder` | A reference to where the real contact info lives (a CRM row, a private note) — never the actual email/handle, in this repo |
| `source_channel` | Where they were recruited from (see `recruitment-criteria.md`'s sourcing pool) |
| `recruited_date` | When outreach was sent |
| `consent_language_agreed_date` | When `consent-language.md` was agreed to (see `study-procedure.md`) |
| `q1`–`q7` | One column per `recruitment-criteria.md` qualification checklist item, yes/no |
| `qualified_yn` | Overall qualification result — yes only if every `q1`–`q7` is yes |
| `disqualify_reason` | If not qualified, which `recruitment-criteria.md` disqualifier applied |
| `install_success_yn` | Did `libra-governor install` + `doctor` come back healthy |
| `doctor_healthy_yn` | No error-severity `doctor` findings at kickoff |
| `evidence_consent_run_date` | When they ran `evidence-report consent` (separate from `consent_language_agreed_date` — see `study-procedure.md`) |
| `evidence_export_received_yn` | Did the founder actually receive an exported file from them |
| `export_task_count` … `export_completed_task_count` | Copied verbatim from their exported JSON's `aggregates` object — never estimated |
| `wtp_signal_summary` | A short pointer to (not a rewrite of) their verbatim willingness-to-pay answer — keep the full verbatim text in the analysis report, not truncated here |
| `dropoff_yn` / `dropoff_reason` | Per `study-procedure.md`'s operational definition of drop-off |
| `midpoint_checkin_date` / `endpoint_checkin_date` | Actual dates the check-ins happened |
| `interview_completed_yn` | Whether the `interview-followup-questions.md` conversation happened |
| `notes` | Anything else worth remembering about this participant |

## Usage

One row per recruited prospect — including ones who never qualified or
never started, so the recruitment funnel in
`analysis-report-template.md` can be computed honestly from real
counts, not reconstructed from memory.
