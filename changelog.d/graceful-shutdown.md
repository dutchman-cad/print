### Command line and install

- Release approved Snapmaker 2.0 tokens and Duet standalone connections, send MQTT
  DISCONNECT to Bambu printers, and close PrusaLink, Elegoo SDCP and Snapmaker J1 sockets when
  the server stops or a CLI command finishes. Cleanup has a three-second overall
  deadline and sends no machine actions; saved jobs and safety state are retained.
