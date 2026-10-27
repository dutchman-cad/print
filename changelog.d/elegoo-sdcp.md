### Command line and install

- Add Elegoo SDCP printers (`kind = "elegoo-sdcp"`) with a required `profile`: `resin`
  follows the SDCP V3 specification and adds printer Stop (command 130, sent once, with its
  acknowledgement and a STOPPED report for the same task awaited); `centauri` (Centauri
  Carbon) is monitoring only. Status comes over one WebSocket per printer with status
  requests, `ping` keepalives, freshness, reconnect backoff, and a fresh report required after
  every reconnect. Each profile has its own status mapping. Preparation, Start, pause, and
  resume are refused, and these printers need no slicer.
