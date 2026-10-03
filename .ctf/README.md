# WROSECODE CTF workspace

Everything a solve produces lands here so the project root stays clean:

```
.ctf/
├── flags.log     # timestamped, source-attributed flag hits (created at runtime)
├── errors.log    # provider, tool, and panic diagnostics (created at runtime)
├── artifacts/    # extracted files, payloads, and challenge evidence
├── solvers/      # repeatable solve scripts the agent can re-run
└── reports/      # writeups, metrics.json / metrics.csv from /export
```

## Flags

`[ctf].flag_patterns` in `config.toml` decides what counts as a flag. Every
model response and tool result is scanned in plain text, ROT13, hex, and Base64.
A hit is appended to `flags.log`, copied to the clipboard when
`[ctf].auto_copy = true`, and surfaced in the dashboard as `🚩 FLAG ALERT`.

## Submitting to CTFd

Set `auto_submit = true` under `[ctf]` in `config.toml`, or export:

```bash
export WROSECODE_CTFD_AUTO_SUBMIT=1   # overrides config.toml either way
export CTFD_URL=https://ctf.example
export CTFD_TOKEN=your-token
export CTFD_CHALLENGE_ID=42
```

`wrosecode ctfd list|scoreboard|download N|submit N 'flag{...}'` works without
the TUI when you want to drive submissions by hand.
