Fixed

- **Missed mutants through run 43385435784 are caught or removed.** Header-walk, event, and exit journeys fail when those operators change a session result. Compares no session reads, and log lines the suite cannot see, are gone or skipped.
