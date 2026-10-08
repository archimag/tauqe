# 6. Code Review

The **Review** tab (`Ctrl+4`) turns a code review into a structured, interactive document instead of a stream of chat text.

## Running a review

1. Add the files to review to the Context (`Ctrl+2`).
2. Open the Review tab and press `r`.
3. The confirmation dialog shows the number of files, the estimated token count and the model. Use `↑/↓` or `Tab` to pick another model and type optional extra instructions (for example, a focus on architecture or security).
4. Press `Enter`. The model receives only the file contents: no history and no repo map, and there are no discovery rounds. The reasoning stream and the draft answer are visible while it works. `Esc` cancels.

The cost of the review is added to the session total and shown as `Prev` in the footer. The result is saved in the project's `.tauqe` directory and restored on the next start.

## Working with findings

Findings are collapsed by default. `Tab` / `Space` folds and unfolds, `t` cycles the status (TODO → DONE → REJECTED), `s` toggles filtering to hide closed findings (DONE / REJECTED) or show all, `x` checks or unchecks a finding, and `c` / `y` / `Enter` copies the finding to the clipboard.

### Discussing findings in Develop

Once a review report is generated, you can selectively discuss and resolve findings with the working model:

1. **Select findings:** Press `x` on one or more findings to check them (marked with `[x]`).
2. **Switch to Develop:** Return to the primary **Develop** tab (`Ctrl+1`).
3. **Prompt the working model:** All checked findings are automatically injected into the working model's context under `<review_findings>`. You can now instruct the model to fix, refactor, or explain the selected issues (e.g. *"Fix the memory leak identified in the review"* or *"Address the checked review findings"*).
4. **Token economy:** When no findings are checked, the review report is completely omitted from the Develop prompt, preventing unnecessary context consumption.
