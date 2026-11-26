# Local assistant quality eval

Run from a checkout with Python 3.10+ and a running enzimOS inference service
that lists the three model IDs in `GET /v1/models`. Use explicit model IDs to
compare the same weights for each request.

```sh
socket="$XDG_RUNTIME_DIR/enzimos-inference/service.sock"
python3 tools/local-eval/eval.py --socket "$socket" --variant baseline --output baseline.jsonl
python3 tools/local-eval/eval.py --socket "$socket" --variant final --output final.jsonl
```

The eval sends synthetic prompts through chat completions. It executes file
calls against a fresh in-memory fixture for each case. It does not run commands,
touch a workspace on disk, or modify inference preferences. Use `--model`,
`--category`, and `--case` to select a subset. Each line of the output records a
case, assistant completions, parsed calls, write results, and scores. The
terminal summary gives per-model and per-category counts.

- Correctness uses expected answers and required or forbidden facts. Writes also
  require the exact requested final file contents.
- Format checks line and word limits and exact reply shapes where specified.
- Tool scores check arguments against the advertised schema and require the
  expected sequence of tool names and workspace paths. An expected missing file
  must produce a valid read call. A text task must have no tool calls.
- A case passes when all three checks pass. Regex checks approximate meaning.
  Review response text for mistakes outside the rubric.

The baseline sends the legacy assistant prompt and four workspace tools using
the service's sampling defaults. The final variant reads the shipped local
prompt, schema, and sampling configuration. Qwen3.5 write cases use the legacy
workspace prompt, schema, and sampling because the optional digest regressed
creates and replacements. The runner uses the case's expected
tool use to select the text or file profile. Exercise automatic intent and
model selection separately through `turn.start` on the harness service.

The intervening variants isolate prompt changes (`prompt`), tool examples
(`examples`), a shorter tool menu (`schema`), sampling (`decode`), and dropping the
tool menu for plain chat (`gated`). `thinking` toggles thinking on against the gated
profile. `required` tests the template grammar with a forced first call.

To run on disposable llama.cpp servers, use pinned runtime revision
`08c252e221759e0f69e80f0450b295c92d2344e1` with the same GGUFs. Start a server for
each model with `--alias MODEL_ID --host SOCKET_DIR/MODEL_ID.sock --jinja`,
`--ctx-size 4608 --parallel 1 --min-p 0 --temp 0.7 --top-p 0.95` and CPU options
matching the service. Use `--socket-dir SOCKET_DIR` in place of `--socket`. The
optional `--cache-prompt` reuses prompt prefixes on disposable servers. Use the
same cache mode for both variants. Generation uses seed 42, up to 256 output
tokens per call and up to five completions per case. Use `--max-tokens` to
set the same output cap in both arms of a shorter targeted experiment.
