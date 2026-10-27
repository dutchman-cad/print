### Command line and install

- Add Snapmaker U1 printers (`kind = "snapmaker-u1"`) through the Moonraker backend, with
  per-tool review of Klipper's `extruder`, `extruder1`, and further tools, the active tool's
  temperature, a check that each job selects one installed tool whose nozzle matches the
  sliced value, and refusal of jobs using several tools, since loaded material per tool is
  not reported.
