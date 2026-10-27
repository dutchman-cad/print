### Command line and install

- Add Repetier-Server printers (`kind = "repetier"`), one per `slug`, with the API key sent
  only as the `x-api-key` header. Status maps `listPrinter` `online`, `active`, and `job`
  to offline, idle, printing, or error, with active extruder and bed temperatures. Start
  uploads a uniquely named model with a size check and cleanup, saves the attempt, then
  sends `copyModel` with `autostart` false and `startJob` once each. Cancel and Stop send
  `stopJob` only for the job this companion started; Stop of other jobs, pause, and resume
  are refused, since the manual documents no pause or paused state and `stopJob` names no
  job. A configured `nozzle_diameter` must match the sliced G-code.
