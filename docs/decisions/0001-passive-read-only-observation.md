# ADR-0001: Passive, Read-Only Observation

**Status:** Accepted product constraint for the initial release<br>
**Date:** 2026-10-27

## Context

AI coding agents are already launched and managed by command-line tools and orchestrators. A separate monitoring application should improve visibility without requiring changes to stable workflows. Interposition through wrapper commands or hooks would increase integration effort and potentially change PTY, terminal, permission, or lifecycle behavior.

## Decision

AgentScope will initially discover and read persisted provider sessions and observe local OS processes **without controlling them**. It will not install hooks, replace CLI commands, rewrite session stores, change permissions, or inject inputs. This includes no launch/stop/restart/approval features in the MVP.

Every displayed lifecycle label or usage number must retain its evidence class. If passive data cannot prove a claim (crash, completion, parent session, token price), the UI will show a qualified or unknown value rather than infer an unsupported fact.

## Consequences

**Benefits:** Minimal impact on existing workflows, provider independence, simpler rollback, local privacy, and low adoption friction.

**Limitations:** Some provider sessions are unsaved, completion/exit codes may be lost, process-to-session matching may remain uncertain, and not all token/cost metrics will be present. AgentScope cannot guarantee visibility into every active process without provider integration.

## Revisit conditions

Reconsider integration only if passive collection cannot meet a clearly measured product requirement. Any future opt-in hooks, wrappers, or remote collectors must be a separate decision with security and backward-compatibility analysis.
