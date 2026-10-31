### Command line and install

- Add optional scenario settings to simulated printers (`offline`, `fault`, `external_job`,
  `fail_after_seconds`, and `bed_sensor`), so farms and the SolidCraft plugin can be tried
  against an offline printer, an error or attention state, a job Print did not start, a job
  that fails, and a printer with a bed sensor. Printers without these keys behave as before.
