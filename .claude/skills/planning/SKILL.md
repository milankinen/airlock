---
name: planning
description: Plan implementation strategy for non-trivial tasks. Use before implementing features, refactors, or multi-file changes.
---

# Planning

**First**: invoke `EnterPlanMode` to activate the built-in plan mode.
This restricts you to read-only tools until the plan is approved,
preventing premature changes.

Then follow these steps within plan mode.

**Skip or abbreviate planning for trivial changes** such as fixing
typos, changing constants, adding log lines, or single-file edits
where the change is obvious. Use judgement — if the change is
straightforward and low-risk, proceed directly without plan mode.

## 1. Understand the user's intention

Understand the user's real intention and requirements before
starting the actual planning. If the user's prompt is too vague
to grasp the reason for the changes, **ask clarifying questions
before proceeding**.

## 2. Understand the affected codebase

Understand the parts of the codebase that will be affected by
the change before planning. Focus on the modules, types, and
interaction patterns that the change touches — not the entire
project. Invest effort proportional to the complexity of the task:

- Delegate exploration to sub-agents when needed:
  - **Opus**: architectural analysis, design trade-offs, complex logic
  - **Sonnet**: reading and summarizing code, moderate reasoning
  - **Haiku**: file searching, listing, simple lookups
- Iterate until the affected surface area is clear

## 3. Challenge the requirements

The user may not have full visibility into the current codebase.
If the requirements conflict with existing solutions, raise this
before planning:

- Explain what conflicts and why
- Propose alternative solutions with reasoning

If the user still wants to proceed with their original vision,
respect that — user is in control.

## 4. Write the plan

- **Phasing**: split the plan into sensibly sized, independent
  tasks. They don't need to be atomic, but each should be
  logically coherent.
- **Two-level structure**:
  - *High-level plan*: describe added/changed abstractions,
    quality characteristics (e.g. performance, security), and
    interaction patterns. May reference type or module names.
  - *Detailed plan*: a complete implementation instruction
    covering the required changes, implementation order, and
    verification gates. This plan should be ready to hand off
    to sub-agents during implementation.
- **Review**: have a sub-agent review the plan for feasibility,
  completeness, and conflicts with existing code.
- **Iterate**: refine the plan based on review findings.

## 5. Present the plan

Present both the high-level summary and the detailed plan to
the user. If the user requests changes, go back to step 4 and
iterate until approved.

## 6. Save the plan

After user approval and before starting implementation, save the    
plan to: `docs/plans/<YYYY-MM-DD>-<title>.md`                           
                                                                    
IMPORTANT: The built-in plan mode uses a temporary file under           
`.claude/plans/`. After ExitPlanMode is approved, you MUST copy
the plan content to `docs/plans/<YYYY-MM-DD>-<title>.md` as your
FIRST action before writing any code. Do not skip this step.            
                                                                    
The key issue is making it clear this is a post-approval action that    
happens after exiting plan mode (when write permissions are restored),
not during plan mode where only the .claude/plans/ file is writable.
