# 6. Structured Code Review

The **Review** workspace (`Ctrl+3`) transforms code review from an informal chat conversation into a structured, verifiable engineering discipline.

---

## 6.1 The Core Dilemma: Generation Speed vs. Human Cognitive Bandwidth

Modern AI models can synthesize code orders of magnitude faster than a human engineer can realistically read, understand, and verify line-by-line. 

At that speed, requiring the developer to manually inspect every generated diff hunk ceases to be a meaningful safety mechanism. It turns into a cognitive bottleneck and induces **review fatigue**, where the human inevitably starts rubber-stamping changes without genuine comprehension.

Conversely, letting the model generate hundreds of lines unsupervised without independent verification leads to catastrophic architectural drift, subtle logic regressions, and security vulnerabilities.

### The Structured AI Code Review Paradigm
TAUQE resolves this dilemma by treating code review as an **independent, structured verification loop**:

```text
       AI Implementation (Develop)
                   ↓
       Independent AI Review (Review)
                   ↓
          Structured Findings
                   ↓
       Human Engineering Judgment (Triage)
                   ↓
       Focused Execution (Develop)
```

The fundamental philosophy is:
> **Not “AI writes code and nobody reads it”, but “AI writes code, an independent AI inspects it, and the human evaluates the evidence.”**

The developer does not need to read every repetitive line of generated boilerplate. Instead, their mental bandwidth is elevated to where it is most valuable: evaluating **engineering consequences** exposed by the audit — architectural trade-offs, correctness hazards, boundary edge-cases, missing tests, and security debt.

---

## 6.2 Architectural Rationale: Complete Isolation of Review

When you trigger a review in TAUQE, the review model does **not** receive the ongoing Develop conversation history, nor does it receive the Tree-sitter Repo Map. It is fed solely the pure content of the selected files.

### Why Review Must Be Isolated:
1. **Elimination of Confirmation Bias (Sycophancy):**
   If the review model shares conversation history with the development model, it inherits the generative narrative, rationalizations, and false assumptions established during code generation. It tends to agree with prior decisions ("looks good to me"). An isolated model approaches the files with zero preconceptions.
2. **Context Window Hygiene & Token Economics:**
   Omitting hundreds of conversational history turns and global repo maps keeps the prompt compact, fast, and cost-effective, allowing higher-capability audit models to concentrate their full attention budget on the code itself.
3. **Model Heterogeneity:**
   TAUQE allows selecting a completely different model for review (e.g. using an analytical reasoning model for auditing while using a fast coding model for Develop).

---

## 6.3 Findings as an Interactive Backlog (Not a Disposable Chat Stream)

Unstructured markdown reports generated in chat windows are quickly scrolled past and forgotten. TAUQE parses review output into **discrete, persistent findings**:
- **Persistent Storage:** Saved in `.tauqe/reviews/` and preserved across sessions.
- **Triage States:** Each finding can be marked as `TODO`, `DONE`, or `REJECTED` (cycled via `t`).
- **Display Filtering:** Press `s` to toggle filtering, hiding resolved items and focusing only on open engineering issues.

---

## 6.4 The Review Workflow in Practice

1. **Curate Context:** In Context (`Ctrl+2`), mark the files you want to review as active.
2. **Initiate Review:** Switch to the Review tab (`Ctrl+3`) and press `r`.
   - The confirmation dialog displays file count, estimated token budget, and active review model.
   - You can optionally provide targeted instructions (e.g. *"Focus strictly on concurrency hazards and memory allocations"*).
3. **Audit and Triage:**
   - Navigate findings using `j` / `k` or `↑` / `↓`.
   - Fold/unfold details with `Tab` or `Space`.
   - Cycle status (`TODO` → `DONE` → `REJECTED`) with `t`.
   - Press `c` / `y` (or `Enter`) to copy an individual finding to the system clipboard.

### Selective Resolution in Develop
Rather than feeding an entire 15-point review dump back to the coding model (which overwhelms its attention window and causes scattered, chaotic edits), you select specific tasks:

1. Press `x` on the relevant finding(s) to mark them with `[x]`.
2. Return to the Develop tab (`Ctrl+1`).
3. The selected findings are automatically injected into the model's prompt under `<review_findings>`.
4. Instruct the model:
   > *"Address the checked review findings."*
5. When no findings are checked, review context is completely excluded from Develop, preserving optimal context hygiene.
