### Command line and install

- Add experimental, opt-in Bambu LAN control using a versioned start profile, verified
  FTPS staging, explicit AMS mappings, and tracked job pause/resume/stop. Keep control
  disabled unless `lan_control = true`; reconcile uncertain starts without resending.
  Verified against simulated printers only; check your own printer before relying on it.
- Check active HMS alerts, sliced nozzle requirements, and loaded filament before Bambu
  upload. Allow bed acknowledgement and another job after FINISH/FAILED, distinguish
  historical terminal errors, and keep delta telemetry available during reconnect pacing.
