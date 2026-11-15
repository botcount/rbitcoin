Changed

- **An outstanding block keeps its peers in one list.**
  A request used to keep three small hash tables: who is racing, when each
  peer was asked, and who was dropped from the race but still holds the
  getdata. Those peers now share one list.
