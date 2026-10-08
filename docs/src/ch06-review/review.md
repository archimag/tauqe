# 6. Code Review

The **Review** tab (`Ctrl+4`) turns a code review into a structured, interactive document instead of a stream of chat text.

## Running a review

1. Add the files to review to the Context (`Ctrl+2`).
2. Open the Review tab and press `r`.
3. The confirmation dialog shows the number of files, the estimated token count and the model. Use `↑/↓` or `Tab` to pick another model and type optional extra instructions (for example, a focus on architecture or security).
4. Press `Enter`. The model receives only the file contents: no history and no repo map, and there are no discovery rounds. The reasoning stream and the draft answer are visible while it works. `Esc` cancels.

The cost of the review is added to the session total and shown as `Prev` in the footer. The result is saved in the project's `.tauqe` directory and restored on the next start.

## Working with findings

Findings are collapsed by default. `Tab` / `Space` folds and unfolds, `t` cycles the status (TODO → DONE → REJECTED), `s` toggles filtering to hide closed findings (DONE / REJECTED) or show all, `x` checks a finding so that it is passed to the working model on the Develop tab, and `c` / `y` / `Enter` copies the finding to the clipboard. When no finding is checked, the review does not appear in the Develop context.
