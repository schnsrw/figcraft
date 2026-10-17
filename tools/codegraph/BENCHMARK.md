# codegraph benchmark

Repository HEAD `6976d4e67565`; machine-local wall-clock times (median of 5 where repeated).

## Indexing (fresh worktree of HEAD, syntax layer)

| Run | Files parsed | Time |
|---|---:|---:|
| Initial full index (912 files, 21303 symbols, 345519 refs) | 912 | 2076 ms |
| No changes | 0 | 149 ms |
| 1 file edited (largest file) | 1 | 210 ms |
| 10 files edited (largest files) | 10 | 497 ms |
| 1 file deleted | 0 parsed, 1 deleted | 175 ms |

Database after the initial index: 49.3 MiB (syntax only).

## Retrieval: git grep vs graph (12 symbols)

Each row is the same question asked both ways. Bytes are what the agent would read back; tokens are estimated at 4 bytes per token. This measures tool output only, not end-to-end agent token use.

| Task | grep: median ms | grep: tokens (median) | graph: median ms | graph: tokens (median) |
|---|---:|---:|---:|---:|
| Find the definition | 145.7 | 24 | 26.7 | 217 |
| Find callers / uses | 18.6 | 158 | 17.6 | 158 |
| Understand a symbol (grep + read the defining file vs symbol context) | 145.7 | 9054 | 48.3 | 427 |
| Change impact of the defining file | 15.7 | 9200 | 20.1 | 724 |

`get_task_context` (2,000-token budget): median 82.3 ms, 1998 tokens returned.

Symbols: `insets`, `is_word`, `refresh_server_libraries`, `divider`, `preview_id`, `stop_id`, `publish_open`, `boolean_steps`, `leptos`, `jobs_for`, `validate_upload_request`, `poll_pads`

## Reference accuracy against SCIP (rust-analyzer)

Pooled over 12 symbols with SCIP data; 60 SCIP reference lines are the reference set.

| Method | Lines returned | Precision | Recall |
|---|---:|---:|---:|
| SCIP (graph, resolved) | 60 | 100% (reference) | 100% (reference) |
| Syntax name matches (graph without SCIP) | 54 | 88.9% | 80.0% |
| `git grep -w name` | 155 | 38.7% | 100.0% |

Syntax definitions with a matching SCIP definition (same name and line), in SCIP-covered files: 19394 of 20208 (96.0%).

## Semantic pass (measured separately, same machine)

| Step | Time | Peak memory |
|---|---:|---:|
| `codegraph semantic` end to end (rust-analyzer SCIP of the workspace + import of 893 documents, 256,357 references, 19,394 definitions) | 177 s | 6.6 GB (rust-analyzer) |
| Import only, from an existing SCIP file (`--scip`) | 31 s | 0.56 GB |

Database with SCIP data: 74 MiB. The semantic pass is occasional (after large changes); the
syntax index is what stays current on every run.

## Reading these numbers

- The graph returns far less text when the question is "what is this and who uses it": symbol
  context is ~430 tokens instead of grepping and reading the defining file (~9,000), and change
  impact ~720 tokens instead of ~9,200 lines of grep output.
- For a plain definition lookup grep is cheaper in bytes (its single matching line is shorter than
  the graph's ranked list with freshness line); the graph is faster.
- Without SCIP, syntax name matches are much more precise than `grep -w` (89% vs 39%) but miss
  about a fifth of references (functions passed as values, uses through re-exports and macros).
  With SCIP data current, references and callers are resolved.
- These are tool-output sizes for equivalent questions, not measured end-to-end agent token savings.
