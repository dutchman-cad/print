### Reports

- Printer JSON now includes required `controls` listing configured machine action support.
  Unsupported confirmations and actions return 409 before reading printer status, including
  bed clearance on monitoring-only printers and printer-level Stop on Repetier-Server.
