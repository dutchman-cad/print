### History

- Keep the newest 10,000 finished-job and bed-clear records in a private, durable JSON Lines
  journal. Add filtered history reads to API v1 and the command line, retain history across
  restarts and job expiry, and state unresolved restart outcomes explicitly.
- Write history on a best-effort basis without affecting printer actions or status reads,
  and distinguish repeated job ids by printer and start time.
