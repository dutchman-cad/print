### Command line and install

- Add monitoring-only Snapmaker 2.0 printers (`kind = "snapmaker-2"`) over the published HTTP
  API: the connection is approved on the touchscreen within a bounded wait, the token stays
  in memory and out of every response, `IDLE` is idle, and any other status is an error with
  the value shown.
- Add monitoring-only Snapmaker J1 and Artisan printers (`kind = "snapmaker-j1"`) over SACP on
  TCP: only module-info and machine-info requests are sent, packets are used only after their
  CRC-8, checksum, and sequence validate, a printing module reads busy, and CNC, laser, or
  unknown modules read attention. Preparation, Start, pause, resume, cancel, and Stop are
  refused for both kinds, and they need no slicer.
