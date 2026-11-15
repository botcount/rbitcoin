Changed

- **A dropped in-flight pack takes the next minimum from each surviving pack's lowest id.**
  The pack's id list is walked only when that id is already gone.
