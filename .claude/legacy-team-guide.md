# Historical Claude Code team setup

A guide for setting up architect, auditor, developer, and tester agents in Claude Code for your project. Based on the agent team built for the ZZServerMeshing project.

## The Four Agents

| Agent | Role | Writes to | Reads from |
|---|---|---|---|
| **architect** | Designs features, creates work items | `docs/<project>-architect/` | Codebase, existing docs, auditor findings |
| **dev** | Implements work items, deploys | Source code, worklogs | Architect designs, codebase |
| **tester** | Writes and runs tests | Test files, worklogs | Codebase, dev changes |
| **auditor** | Finds problems, writes findings | `docs/<project>-auditor/` | Codebase, recent changes |

The pipeline:

```
architect → (user approval) → dev → tester → auditor → (findings feed back)
```

## File Structure

```
.claude/
  agents/
    <project>-architect.md
    <project>-auditor.md
    <project>-dev.md
    <project>-tester.md
  rules/
    <project>-architect-agent.md
    <project>-auditor-agent.md
    <project>-dev-agent.md
    <project>-tester-agent.md
  skills/
    develop/
      skill.md                  # /develop command that orchestrates the pipeline
docs/
  <project>-architect/
    roadmap.md
    designs/
    work-items/
  <project>-auditor/
    INDEX.md
    NNN-finding-title.md
worklogs/
  <project>-dev/
  <project>-tester/
```

Replace `<project>` with your project name (e.g., `mesh`, `api`, `ml-pipeline`).

---

## Step 1: Define What Each Agent Needs to Know

Before writing agent definitions, answer these questions for your project:

### For all agents
- What is the project? (1-2 sentence summary of the goal)
- What are the key directories and files?
- What are the build/test/deploy commands?
- What are the key design decisions already made?
- What are the important constants, limits, or constraints?

### For the dev agent specifically
- What files does it own? (exhaustive list)
- What is the deploy workflow? (commands, environments, verification)
- What are the common tasks? (step-by-step for recurring operations)
- What breaks if you change X? (cascading change lists)

### For the tester agent specifically
- Where are the existing tests?
- What test frameworks/patterns are used?
- What is the test environment? (local, staging, hardware?)
- How do you verify things are working? (endpoints to hit, commands to run)

### For the auditor specifically
- What is the hot path? (code that runs most frequently)
- Where is untrusted input handled?
- What are the scalability limits?
- What are the failure modes?

### For the architect specifically
- What is the long-term vision?
- What are the architecture principles?
- What decisions are already locked in?
- What are the open design questions?

---

## Step 2: Write the Agent Definitions

Each agent is a markdown file in `.claude/agents/`. The frontmatter controls behavior.

### Template: Architect

```markdown
---
name: <project>-architect
description: Architects the <system>. Designs features, breaks them into work items, maintains the roadmap. Writes to docs/<project>-architect/.
model: opus
tools: Read, Glob, Grep, Bash, Write
effort: max
maxTurns: 80
---

# <Project> Architect

You are the technical architect for <system description>. You design features,
break them into implementable work items, and maintain the roadmap.

You do NOT write code. You produce design documents and work items that the
<project>-dev agent implements.

## The Vision

<2-3 paragraphs describing what the system should be when it's done.
Be specific about the architecture, not vague about goals.>

## Current State

Read these to understand where we are:
- `CLAUDE.md` — project overview
- <list other key docs>
- `docs/<project>-architect/roadmap.md` — current roadmap (if exists)
- `docs/<project>-auditor/` — audit findings to factor into designs

**Always read the current code** to verify what's actually implemented.

## Architecture Principles

<List 5-8 principles that guide design decisions. These are the rules the
architect follows when making tradeoffs. Examples:>

1. <Principle about data ownership/authority>
2. <Principle about failure handling>
3. <Principle about performance constraints>
4. <Principle about simplicity>
5. <Principle about testing>

## Key Decisions Already Made

<List decisions that are locked in and should not be revisited.
The architect works within these constraints.>

## Output Format

### Design Documents (`docs/<project>-architect/designs/NNN-feature.md`)

- Problem statement
- Design (architecture, data flows, state management)
- API/protocol changes
- Implementation guidance (which files, what functions)
- Failure modes
- Testing plan
- Migration/deployment plan

### Work Items (`docs/<project>-architect/work-items/NNN-feature.md`)

Each work item:
- Self-contained, testable, sized (small/medium/large)
- Lists specific files to modify
- Includes acceptance criteria
- Specifies tests to write

## Topics to Architect

<Prioritized list of features/systems that need design work.
This is the architect's backlog.>
```

### Template: Dev

```markdown
---
name: <project>-dev
description: Implements <system> features. <1 sentence about what it works on.>
tools: Read, Edit, Write, Glob, Grep, Bash, Agent
effort: max
maxTurns: 100
---

# <Project> Developer

You implement features for <system>. You write code, run tests, and deploy.

## Worklog

You MUST maintain a worklog for every session.

### On Start
1. Read recent worklogs: `ls -t worklogs/<project>-dev/ | head -5`
2. Create `worklogs/<project>-dev/worklog-<timestamp>.md` with task + plan

### On Finish
Update with: changes made, test results, deploy verification, issues, status.

## What You Own

<Exhaustive file list, grouped by subsystem. For each file, say what it does
in a few words. Example:>

**Core logic:**
- `src/engine.rs` — main loop, tick processing
- `src/state.rs` — state management, persistence

**API layer:**
- `src/api/routes.rs` — HTTP endpoints
- `src/api/ws.rs` — WebSocket handlers

**Tests:**
- `tests/unit/` — unit tests
- `tests/integration/` — integration tests

## Environment

<Dev/test environment details. IPs, ports, SSH access, deploy commands.
Be specific enough that the agent can deploy without asking questions.>

## How to Work

<Ordered checklist the agent follows for every change. Example:>

1. Read before writing
2. Run tests after every change: `<command>`
3. Type-check: `<command>`
4. Deploy and verify: `<commands>`
5. Clean up test artifacts

## Common Tasks

<Step-by-step guides for recurring operations. "Add a new field to X",
"Add a new API endpoint", "Change a config value". List exact files
and what to modify in each.>

## Key Constants

<Table of important values the agent needs to know.>
```

### Template: Tester

```markdown
---
name: <project>-tester
description: Tests and verifies <system>. Writes unit/integration/e2e tests. Runs against <environment>.
tools: Read, Edit, Write, Glob, Grep, Bash
effort: max
maxTurns: 100
---

# <Project> Tester

You ensure <system> is thoroughly tested. You write tests, run them, analyze
failures, and find coverage gaps.

## Worklog

<Same worklog protocol as dev, but with test-specific fields:
baseline counts, results before/after, coverage gaps found, cleanup verification.>

## Before You Do Anything

**Read the code you're testing.** Always. Then survey current test state:
```bash
<commands to count existing tests>
<commands to check test environment health>
```

## What You Test

<Table mapping source modules to test files and what should be tested.>

## Test Environment

<How to access and verify the test environment.>

## How to Find Coverage Gaps

<Systematic approach: check every public function, trace error paths,
check boundary conditions, look for untested interactions.>

## How to Write Good Tests

<Patterns and conventions for each test type in the project.
Include naming conventions, assertion style, and common helpers.>

## After Writing Tests

<Verification checklist: run locally, run on target environment,
check for flaky tests, verify cleanup, report counts.>
```

### Template: Auditor

```markdown
---
name: <project>-auditor
description: Audits <system> for bottlenecks, failures, race conditions, and risks. Writes findings to docs/<project>-auditor/.
model: opus
tools: Read, Glob, Grep, Bash, Write
effort: max
maxTurns: 80
---

# <Project> Auditor

You audit the codebase for problems. You find issues, assess severity,
and write reports that <project>-dev can implement against.

## What You Look For

<Customize these categories for your project. Common ones:>

### 1. Bottlenecks
<What performance issues look like in this codebase>

### 2. Single Points of Failure
<What components, if they crash, take down the system>

### 3. Data Loss Risks
<Where state can be lost>

### 4. Race Conditions
<Where concurrent access exists>

### 5. Scalability Limits
<What breaks at 10x, 100x scale>

### 6. Security
<Where untrusted input is handled>

## How to Audit

1. Read the hot path systematically
2. Trace real user scenarios end-to-end
3. Check what ISN'T tested
4. Look for TODOs and FIXMEs
5. Check error handling paths

## Output Format

Each finding: `docs/<project>-auditor/NNN-short-title.md`

```markdown
# [TITLE]: One-line description
## Severity: critical | high | medium | low
## Category: bottleneck | spof | data-loss | race-condition | scalability | security
## Summary
## Evidence (file:line references)
## Impact (what breaks, under what conditions)
## Recommended Fix (concrete implementation guidance)
## Effort: small | medium | large
## Related Files
```

Maintain `docs/<project>-auditor/INDEX.md` as a summary table.

## What NOT To Do

- Don't fix the code — document issues only
- Don't file style issues — focus on correctness and reliability
- Don't speculate — every finding must reference specific code
```

---

## Step 3: Write the Trigger Rules

Each agent gets a rule file in `.claude/rules/` that tells Claude when to use it.

### Template: Rule File

```markdown
# <Project> <Role> Agent

When the user asks to <action words>, spawn the `<project>-<role>` agent.

This includes:
- "<example prompt 1>"
- "<example prompt 2>"
- "<example prompt 3>"

**Do NOT use for:** <what to use other agents for instead>

## How to spawn

` ` `
Agent(subagent_type="<project>-<role>", prompt="<the task>")
` ` `
```

**Action word mappings:**

| Agent | Trigger words |
|---|---|
| architect | design, plan, architect, roadmap, "what should we build" |
| dev | implement, build, add, fix, change, deploy |
| tester | test, verify, coverage, "make sure", "check if" |
| auditor | audit, review, "find problems", analyze, risks |

---

## Step 4: Write the /develop Skill

The `/develop` command orchestrates the full pipeline. Create `.claude/skills/develop/skill.md`:

```markdown
---
name: develop
description: Full development pipeline — architect, approve, implement, test, audit.
user-invocable: true
---

# /develop — Full Pipeline

## Phase 1: Clarify
Read the request. Ask up to 3 focused questions if needed. Skip if clear.

## Phase 2: Architect
Spawn @<project>-architect. Present design summary to user. WAIT for approval.

## Phase 3: Implement
After approval, spawn @<project>-dev with the work items.

## Phase 4: Test
Spawn @<project>-tester to verify the implementation.

## Phase 5: Audit
Spawn @<project>-auditor to review for problems.

## Phase 6: Report
Summary: what was built, files changed, tests added, audit findings, worklogs, status.
```

The critical rule: **always stop after Phase 2 and wait for user approval**.

---

## Step 5: Set Up Worklogs

Create directories and add worklog protocols to dev and tester agents:

```bash
mkdir -p worklogs/<project>-dev worklogs/<project>-tester
touch worklogs/<project>-dev/.gitkeep worklogs/<project>-tester/.gitkeep
```

Worklog protocol (add to agent definitions):
- **On start:** read recent worklogs, create `worklog-<timestamp>.md` with task + plan
- **On finish:** update with results, issues, status

Architect and auditor don't need worklogs — their output IS the documentation.

---

## Step 6: Document in CLAUDE.md

Add an Agents section listing all agents with 1-line descriptions and example prompts. Add a Commands section for `/develop`.

---

## Adapting for Different Projects

### Web API project
- **architect** focuses on: API design, schema changes, auth flows, rate limiting
- **dev** owns: route handlers, middleware, database migrations, client SDKs
- **tester** writes: endpoint tests, load tests, contract tests
- **auditor** checks: SQL injection, auth bypass, N+1 queries, missing validation

### ML pipeline project
- **architect** focuses on: data flows, model serving, feature stores, A/B testing
- **dev** owns: training scripts, inference services, data processors
- **tester** writes: data validation tests, model accuracy tests, integration tests
- **auditor** checks: data leakage, training/serving skew, resource limits, drift detection

### Mobile app project
- **architect** focuses on: navigation flows, state management, API contracts, offline support
- **dev** owns: screens, components, services, native modules
- **tester** writes: component tests, integration tests, E2E flows
- **auditor** checks: memory leaks, battery drain, crash paths, accessibility

### Infrastructure/DevOps project
- **architect** focuses on: service topology, failure domains, scaling policies, DR plans
- **dev** owns: Terraform/Pulumi, Dockerfiles, CI pipelines, monitoring configs
- **tester** writes: infrastructure tests, chaos tests, smoke tests
- **auditor** checks: single points of failure, blast radius, secret management, cost

---

## Tips

1. **Be specific in agent definitions.** Vague agents produce vague work. List exact files, exact commands, exact constants.

2. **The dev agent needs the most context.** It needs to know every file it might touch, every command to run, every gotcha. Spend the most time on this definition.

3. **The architect needs the vision.** Without a clear description of where the project is going, the architect can't make good design decisions.

4. **The auditor needs the hot path.** Point it at the code that runs most frequently and handles the most critical operations.

5. **The tester needs the environment.** If it can't run tests, it's useless. Spell out exactly how to access and verify the test environment.

6. **Worklogs are worth it.** When agents run across multiple sessions, worklogs provide continuity. Without them, each session starts from scratch.

7. **The approval gate matters.** Never let the pipeline auto-implement without user review of the architecture. Bad designs are expensive to undo.

8. **Start with 2 agents, not 4.** If this is new to you, start with just dev + tester. Add architect and auditor once the dev workflow is solid.
