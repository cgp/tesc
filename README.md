# Metrix

A load generator with machine-authored test plans, paired with host and container
observation. Short test windows, easy-to-modify mixtures, and statistics that say
what they can and cannot support.

## Documents

| | |
|---|---|
| [docs/design-api-engine-contract.md](docs/design-api-engine-contract.md) | What each side owns and how they hand off. **Start here.** |
| [docs/design-api.md](docs/design-api.md) | Control plane, observation, front end |
| [docs/design-engine.md](docs/design-engine.md) | Load generation and measurement |
| [docs/implementation-api.md](docs/implementation-api.md) | Track A checklist |
| [docs/implementation-engine.md](docs/implementation-engine.md) | Track B checklist |
| [CHANGELOG.md](CHANGELOG.md) | Running work log |

## Layout

```
engine/    Rust workspace - the load generator, ships alone
api/       Python control plane (uv) and the static front end
schema/    Generated JSON Schemas - the contract between the two
policy/    Read-only IAM policy for ECS discovery
examples/  Worked plan bundles
```

## Status

Skeleton. Track A (observation) is the active work; the engine is designed but
not started.

It should be obvious that the codebase was written with a model, early sept. Larger design decisions were made by humans, and the details were reviewed. The initial design was a launching point, further guidance was only captured in the design docs.

## Development

```bash
cd api && uv sync && uv run metrix-api     # http://127.0.0.1:8080/api/health
cd engine && cargo build

bash scripts/check.sh                     # everything CI runs
bash scripts/check.sh engine              # or one scope: engine | api | contract
```

