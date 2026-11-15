Changed

- **A spend checkpoint keeps page images in the generation file.**
  The live directory stores where each page sits in that file and reads it
  on demand, including an explicit zero. Fold copies those file bytes onto
  `spent.body`. The open set also asks for a checkpoint at 65,536 distinct
  pages and keeps asking until that set drains. Freeze wakes a confirm
  thread blocked on the edge cap before the cooled-page copy. That freeze
  does not seal the new tip and does not clone the page snapshot, so a
  disconnect during the copy still folds a generation sealed at an older
  height. A held generation restored after that freeze does not count
  toward the cap.
