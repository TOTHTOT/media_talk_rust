# mermaid-check

A CLAUDE Code skill for checking and formatting Mermaid diagrams in documentation.

## Prerequisites

Install the Mermaid CLI tool:
```bash
npm install -g @mermaid-js/mermaid-cli
# or
npm install -g mermaid-cli
```

The CLI provides `mmdc` (Mermaid CLI) command.

## Usage

### Check a single file for Mermaid syntax errors

```bash
mmdc -i path/to/file.md -o /dev/null
```

### Check all mermaid blocks in a file

To check Mermaid diagrams without rendering, use the linter approach:

```bash
# Extract and validate mermaid diagrams
grep -A 100 '```mermaid' README.md | grep -B 100 '```' | head -n -1 > /tmp/diagram.mmd
mmdc -i /tmp/diagram.mmd -o /dev/null 2>&1 | head -20
```

Or use `mermaid-lint` if installed:
```bash
npx mermaid-lint "**/*.md"
```

## Common Mermaid Errors

### sequenceDiagram

1. **Self-calls**: `A->>A` is NOT valid in sequenceDiagram
   - Use `Note over A: action` or just `Note over A: description`

2. **Participant syntax**: Use `participant NAME` or `participant NAME as ALIAS`
   - Do NOT use `participant NAME[label]` — that's flowchart syntax

3. **Arrow directions**:
   - `->` solid line (synchronous)
   - `-->>` dashed line (response)
   - `->>` arrow head only
   - `-->o` circle end

4. **Active/deactivate**: Must match existing participants

### flowchart

1. **Node shapes**: Use `[text]`, `(text)`, `{text}` or `[[text]]`
2. **Subgraph**: `subgraph NAME [...] end`
3. **Direction**: TB/TD/LR/RL

### stateDiagram-v2

1. **Transitions**: Use `-->` or `-->` with labels
2. **States**: Use `state NAME` or `state NAME { ... }`

## Quick Check Script

Save as `check-mermaid.sh`:

```bash
```bash
# Quick check (mmdc outputs SVG, so use temp file)
mmdc -i diagram.mmd -o /tmp/out.svg 2>&1 | head -20
```

Or use the project script:
```bash
bash scripts/check-mermaid.sh crates/call-ui/README.md
```

All 13 mermaid blocks validated OK when no error output.
```

## Making Edits

When editing Mermaid in CLAUDE.md or documentation:

1. Use `sequenceDiagram` for call flows
2. Use `flowchart` for architecture/dependency diagrams
3. Use `stateDiagram-v2` for state machines
4. Keep diagrams simple — complex nesting causes errors
5. Use `Note over A,B: text` for self-notes instead of self-calls
