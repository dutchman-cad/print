### Command line and install

- Add farm reporting from the print history: `history --summary` and
  `GET /api/v1/history/summary` give per-printer and total job counts by outcome, success
  rate, total and average print time and last finished job, and `history --csv` and
  `GET /api/v1/history?format=csv` export the filtered records as CSV. JSON stays the default.
