### Command line and install

- Add `dutchman-print init`, which writes a commented, private config for one printer. It asks
  for the printer type, address, credentials, name, id, slicer, and preset names on a terminal,
  takes every answer as a flag for scripts, checks the result with the config loader before
  writing, refuses to replace a file without `--force`, and contacts nothing.
